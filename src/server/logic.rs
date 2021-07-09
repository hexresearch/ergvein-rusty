use super::fee::FeesCache;
use super::metrics::*;
use super::rates::RatesCache;
use crate::filter::*;
use bitcoin_utxo::storage::chain::get_chain_height;
use ergvein_protocol::message::*;
use futures::sink;
use futures::{Future, Sink, Stream};
use rand::{thread_rng, Rng};
use rocksdb::DB;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// Amount of seconds connection is open after handshake
pub const CONNECTION_DROP_TIMEOUT: u64 = 60 * 20;
/// Limit to amount of filters that can be requested via the server in one request
pub const MAX_FILTERS_REQ: u32 = 2000;

#[derive(Debug)]
pub enum IndexerError {
    HandshakeSendError,
    HandshakeTimeout,
    HandshakeRecv,
    HandshakeViolation,
    HandshakeNonceIdentical,
    NotCompatible(Version),
    NotSupportedCurrency(Currency),
}

pub async fn indexer_logic(
    addr: String,
    db: Arc<DB>,
    fees: Arc<Mutex<FeesCache>>,
    rates: Arc<RatesCache>,
) -> (
    impl Future<Output = Result<(), IndexerError>>,
    impl Stream<Item = Message> + Unpin,
    impl Sink<Message, Error = ergvein_protocol::message::Error>,
) {
    let (in_sender, mut in_reciver) = mpsc::unbounded_channel::<Message>();
    let (out_sender, out_reciver) = mpsc::unbounded_channel::<Message>();
    let logic_future = {
        async move {
            handshake(addr.clone(), db.clone(), &mut in_reciver, &out_sender).await?;

            let timeout = tokio::time::sleep(Duration::from_secs(CONNECTION_DROP_TIMEOUT));
            tokio::pin!(timeout);

            let filters_fut = serve_filters(
                addr.clone(),
                db.clone(),
                fees,
                rates,
                &mut in_reciver,
                &out_sender,
            );
            tokio::pin!(filters_fut);

            let announce_fut = announce_filters(db.clone(), &out_sender);
            tokio::pin!(announce_fut);

            let mut close = false;
            while !close {
                tokio::select! {
                    _ = &mut timeout => {
                        eprintln!("Connection closed by mandatory timeout {}", addr);
                        close = true;
                    },
                    res = &mut filters_fut => match res {
                        Err(e) => {
                            eprintln!("Failed to serve filters to client {}, reason: {:?}", addr, e);
                            close = true;
                        }
                        Ok(_) => {
                            eprintln!("Impossible, fitlers serve ended to client {}", addr);
                            close = true;
                        }
                    },
                    res = &mut announce_fut => match res {
                        Err(e) => {
                            eprintln!("Failed to announce filters to client {}, reason: {:?}", addr, e);
                            close = true;
                        }
                        Ok(_) => {
                            eprintln!("Impossible, fitlers announce ended to client {}", addr);
                            close = true;
                        }
                    },
                }
            }

            Ok(())
        }
    };
    let msg_stream = UnboundedReceiverStream::new(out_reciver);
    let msg_sink = sink::unfold(in_sender, |in_sender, msg| async move {
        in_sender.send(msg).unwrap();
        Ok::<_, ergvein_protocol::message::Error>(in_sender)
    });
    (logic_future, msg_stream, msg_sink)
}

async fn handshake(
    addr: String,
    db: Arc<DB>,
    msg_reciever: &mut mpsc::UnboundedReceiver<Message>,
    msg_sender: &mpsc::UnboundedSender<Message>,
) -> Result<(), IndexerError> {
    let ver_msg = build_version_message(db);
    msg_sender
        .send(Message::Version(ver_msg.clone()))
        .map_err(|e| {
            println!("Error when sending handshake: {:?}", e);
            IndexerError::HandshakeSendError
        })?;
    let timeout = tokio::time::sleep(Duration::from_secs(20));
    tokio::pin!(timeout);
    let mut got_version = false;
    let mut got_ack = false;
    while !(got_version && got_ack) {
        tokio::select! {
            _ = &mut timeout => {
                eprintln!("Handshake timeout {}", addr);
                return Err(IndexerError::HandshakeTimeout)
            }
            emsg = msg_reciever.recv() => match emsg {
                None => {
                    eprintln!("Failed to recv handshake for {}", addr);
                    return Err(IndexerError::HandshakeRecv)
                }
                Some(msg) => match msg {
                    Message::Version(vmsg)=> {
                        if !Version::current().compatible(&vmsg.version) {
                            eprint!("Not compatible version for client {}, version {:?}", addr, vmsg.version);
                            return Err(IndexerError::NotCompatible(vmsg.version));
                        }
                        if vmsg.nonce == ver_msg.nonce {
                            eprint!("Connected to self, nonce identical for {}", addr);
                            return  Err(IndexerError::HandshakeNonceIdentical);
                        }
                        println!("Handshaked with client {} and version {:?}", addr, vmsg.version);
                        got_version = true;
                        msg_sender.send(Message::VersionAck).map_err(|e| {
                            println!("Error when sending verack: {:?}", e);
                            IndexerError::HandshakeSendError
                        })?;
                    }
                    Message::VersionAck => {
                        println!("Received verack for client {}", addr);
                        got_ack = true;
                    }
                    _ => {
                        eprintln!("Received from {} something that not handshake: {:?}", addr, msg);
                        return Err(IndexerError::HandshakeViolation);
                    },
                },
            }
        }
    }
    Ok(())
}

fn build_version_message(db: Arc<DB>) -> VersionMessage {
    // "standard UNIX timestamp in seconds"
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time error")
        .as_secs();

    // "Node random nonce, randomly generated every time a version packet is sent. This nonce is used to detect connections to self."
    let mut rng = thread_rng();
    let nonce: [u8; 8] = rng.gen();

    // Construct the message
    VersionMessage {
        version: Version::current(),
        time: timestamp,
        nonce,
        scan_blocks: vec![ScanBlock {
            currency: Currency::Btc,
            version: Version {
                major: 1,
                minor: 0,
                patch: 0,
            },
            scan_height: get_filters_height(&db) as u64,
            height: get_chain_height(&db.clone()) as u64,
        }],
    }
}

fn is_supported_currency(currency: &Currency) -> bool {
    *currency == Currency::Btc
}

async fn serve_filters(
    addr: String,
    db: Arc<DB>,
    fees: Arc<Mutex<FeesCache>>,
    rates: Arc<RatesCache>,
    msg_reciever: &mut mpsc::UnboundedReceiver<Message>,
    msg_sender: &mpsc::UnboundedSender<Message>,
) -> Result<(), IndexerError> {
    loop {
        if let Some(msg) = msg_reciever.recv().await {
            match &msg {
                Message::GetFilters(req) => {
                    println!(
                        "Client {} requested filters for {:?} from {} to {}",
                        addr,
                        req.currency,
                        req.start,
                        req.start + req.amount as u64
                    );
                    if !is_supported_currency(&req.currency) {
                        msg_sender
                            .send(Message::Reject(RejectMessage {
                                id: msg.id(),
                                data: RejectData::InternalError,
                                message: format!("Not supported currency {:?}", req.currency),
                            }))
                            .unwrap();
                        return Err(IndexerError::NotSupportedCurrency(req.currency));
                    }
                    let h = get_filters_height(&db);
                    if req.start > h as u64 {
                        let resp = Message::Filters(FiltersResp {
                            currency: req.currency,
                            filters: vec![],
                        });
                        msg_sender.send(resp).unwrap();
                    } else {
                        let amount = req.amount.min(MAX_FILTERS_REQ);
                        let filters: Vec<Filter> = read_filters(&db, req.start as u32, amount)
                            .iter()
                            .map(|(h, f)| Filter {
                                block_id: h.to_vec(),
                                filter: f.content.clone(),
                            })
                            .collect();
                        FILTERS_SERVED_COUNTER.inc_by(filters.len() as u64);
                        println!(
                            "Sent {} {:?} filters to client {} from {} to {}",
                            filters.len(),
                            req.currency,
                            addr,
                            req.start,
                            req.start + req.amount as u64
                        );
                        let resp = Message::Filters(FiltersResp {
                            currency: req.currency,
                            filters,
                        });
                        msg_sender.send(resp).unwrap();
                    }
                }
                Message::Ping(nonce) => {
                    msg_sender.send(Message::Pong(*nonce)).unwrap();
                }
                Message::GetFee(curs) => {
                    let mut resp = vec![];
                    for cur in curs {
                        if is_supported_currency(cur) {
                            let fees = fees.lock().unwrap();
                            if let Some(f) = make_fee_resp(&fees, cur) {
                                resp.push(f);
                            }
                        }
                    }
                    msg_sender.send(Message::Fee(resp)).unwrap();
                }
                Message::GetRates(reqs) => {
                    let mut resp = vec![];
                    for req in reqs {
                        if is_supported_currency(&req.currency) {
                            if let Some(fiats) = rates.get(&req.currency) {
                                let mut rate_resps = vec![];
                                for fiat in &req.fiats {
                                    if let Some(rate) = fiats.get(&fiat) {
                                        rate_resps.push(FiatRate {
                                            fiat: *fiat,
                                            rate: *rate.value(),
                                        });
                                    }
                                }
                                resp.push(RateResp {
                                    currency: req.currency,
                                    rates: rate_resps,
                                })
                            }
                        }
                    }
                    msg_sender.send(Message::Rates(resp)).unwrap();
                }
                _ => (),
            }
        }
    }
}

async fn announce_filters(
    db: Arc<DB>,
    msg_sender: &mpsc::UnboundedSender<Message>,
) -> Result<(), IndexerError> {
    loop {
        let h = filters_height_changes(&db, Duration::from_secs(3)).await;
        let filters = read_filters(&db, h, 1)
            .iter()
            .map(|(h, f)| Filter {
                block_id: h.to_vec(),
                filter: f.content.clone(),
            })
            .collect();
        let resp = Message::Filters(FiltersResp {
            currency: Currency::Btc,
            filters,
        });
        msg_sender.send(resp).unwrap();
    }
}

fn make_fee_resp(fees: &FeesCache, currency: &Currency) -> Option<FeeResp> {
    match currency {
        Currency::Btc => {
            let f = &fees.btc;
            Some(FeeResp::Btc((
                Currency::Btc,
                FeeBtc {
                    fast_conserv: f.fastest_fee as u64,
                    fast_econom: f.fastest_fee as u64,
                    moderate_conserv: f.half_hour_fee as u64,
                    moderate_econom: f.half_hour_fee as u64,
                    cheap_conserv: f.hour_fee as u64,
                    cheap_econom: f.hour_fee as u64,
                },
            )))
        }
        _ => None,
    }
}
