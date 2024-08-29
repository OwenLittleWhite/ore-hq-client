use base64::prelude::*;
use clap::{ arg, Parser };
use drillx_2::equix;
use futures_util::{ stream::SplitSink, SinkExt, StreamExt };
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use std::{
    ops::{ ControlFlow, Range },
    str::FromStr,
    sync::Arc,
    time::{ Duration, Instant, SystemTime, UNIX_EPOCH },
};
use tokio::net::TcpStream;
use tokio::sync::{ mpsc::UnboundedSender, Mutex };
use tokio_tungstenite::{
    connect_async,
    tungstenite::{ handshake::client::{ generate_key, Request }, http::request, Message },
    MaybeTlsStream,
    WebSocketStream,
};

#[derive(Debug)]
pub enum ServerMessage {
    StartMining([u8; 32], Range<u64>, u64),
}

#[derive(Debug, Parser)]
pub struct MineArgs {
    #[arg(
        long,
        value_name = "CORES",
        default_value = "1",
        help = "Number of cores to use while mining"
    )]
    pub cores: u32,

    #[arg(long, value_name = "address", help = "钱包公钥地址", required = true)]
    pub address: String,

    #[arg(
        long,
        value_name = "cutoff",
        default_value = "30",
        help = "单次最大挖矿时间"
    )]
    pub cutoff: u32,
}

pub struct MineTask {
    pub challenge: [u8; 32],
    pub nonce_range: Range<u64>,
    pub cutoff: u64,
    pub sender: Option<Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>>,
    pub receive_time: SystemTime,
    pub url: String,
}

impl Clone for MineTask {
    fn clone(&self) -> Self {
        MineTask {
            challenge: self.challenge,
            nonce_range: self.nonce_range.clone(),
            cutoff: self.cutoff,
            sender: self.sender.clone(), // Arc already handles cloning of inner data
            receive_time: self.receive_time, // SystemTime is Copy, no special handling needed
            url: self.url.clone(),
        }
    }
}

impl MineTask {
    pub fn new() -> Self {
        MineTask {
            challenge: [0u8; 32],
            nonce_range: 0..0, // 空范围
            cutoff: 0, // 默认值
            sender: None,
            receive_time: SystemTime::now(),
            url: "".to_string(),
        }
    }
}

async fn connect_and_receive(
    address: &str,
    url: String,
    mine_task: Arc<Mutex<MineTask>>,
    request_mine: Arc<Mutex<bool>>,
    last_challenge: Arc<Mutex<[u8; 32]>>
) {
    let pubkey = Pubkey::from_str(address).unwrap();
    loop {
        let base_url = url.clone();
        let mut ws_url_str = { format!("ws://{}", url) };
        if ws_url_str.chars().last().unwrap() != '/' {
            ws_url_str.push('/');
        }
        let client = reqwest::Client::new();

        let http_prefix = "http".to_string();
        println!("Connecting to server {}...", base_url);
        let timestamp = if
            let Ok(response) = client
                .get(format!("{}://{}/timestamp", http_prefix, base_url))
                .send().await
        {
            if let Ok(ts) = response.text().await {
                if let Ok(ts) = ts.parse::<u64>() {
                    ts
                } else {
                    println!("Server response body for /timestamp failed to parse, contact admin.");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            } else {
                println!("Server response body for /timestamp is empty, contact admin.");
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
        } else {
            println!("Server restarting, trying again in 3 seconds...");
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        println!("{} Server Timestamp: {}", url, timestamp);
        let address = address.clone();
        // let ts_msg = timestamp.to_le_bytes();
        // let sig = key.sign_message(&ts_msg);

        ws_url_str.push_str(&format!("?timestamp={}", timestamp));
        let url = url::Url::parse(&ws_url_str).expect("Failed to parse server url");
        let host = url.host_str().expect("Invalid host in server url");

        let auth = BASE64_STANDARD.encode(format!("{}:{}", address, ""));

        println!("Connecting to server {}...", base_url);
        let base_url_clone = base_url.clone();
        let request = Request::builder()
            .method("GET")
            .uri(url.to_string())
            .header("Sec-Websocket-Key", generate_key())
            .header("Host", host)
            .header("Upgrade", "websocket")
            .header("Connection", "upgrade")
            .header("Sec-Websocket-Version", "13")
            .header("Authorization", format!("Basic {}", auth))
            .body(())
            .unwrap();

        match connect_async(request).await {
            Ok((ws_stream, _)) => {
                println!("Connected to network!");
                let (sender, mut receiver) = ws_stream.split();
                let (message_sender, mut message_receiver) =
                    tokio::sync::mpsc::unbounded_channel::<ServerMessage>();

                let receiver_thread = tokio::spawn(async move {
                    while let Some(Ok(message)) = receiver.next().await {
                        if process_message(message, message_sender.clone()).is_break() {
                            break;
                        }
                    }
                });
                let share_sender = Arc::new(Mutex::new(sender));
                let share_sender_clone = share_sender.clone();
                let request_mine = request_mine.clone();
                tokio::spawn(async move {
                    loop {
                        if !*request_mine.lock().await {
                            continue;
                        }
                        {
                            let share_clone = { share_sender_clone.lock().await };
                            let mut sender_clone = share_clone;
                            // send Ready message
                            let now = SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .expect("Time went backwards")
                                .as_secs();

                            let msg = now.to_le_bytes();
                            // let sig = key.sign_message(&msg).to_string().as_bytes().to_v ec();
                            let mut bin_data: Vec<u8> = Vec::new();
                            bin_data.push(0u8);
                            bin_data.extend_from_slice(&pubkey.to_bytes());
                            bin_data.extend_from_slice(&msg);
                            bin_data.extend("xxx".bytes());

                            let _ = sender_clone.send(Message::Binary(bin_data)).await;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                });

                // receive messages
                let message_sender = share_sender.clone();
                let mine_task_clone = Arc::clone(&mine_task);
                let url_clone = base_url_clone.clone();
                while let Some(msg) = message_receiver.recv().await {
                    let url = url_clone.clone();
                    match msg {
                        ServerMessage::StartMining(challenge, nonce_range, cutoff) => {
                            {
                                let last_challenge_lock = last_challenge.lock().await;
                                let last_challenge = *last_challenge_lock;
                                if last_challenge == challenge {
                                    println!("Duplicate challenge received\n");
                                    continue;
                                }
                                let mut mine_task_lock = mine_task_clone.lock().await;
                                if let Some(_sender) = mine_task_lock.clone().sender {
                                    continue;
                                } else {
                                    println!("{} received start mining message, Cutoff {}", url, cutoff);
                                    let _sender = message_sender.clone();
                                    mine_task_lock.challenge = challenge;
                                    mine_task_lock.nonce_range = nonce_range;
                                    mine_task_lock.cutoff = cutoff;
                                    mine_task_lock.sender = Some(_sender);
                                    mine_task_lock.receive_time = SystemTime::now();
                                    mine_task_lock.url = url.to_string();
                                }
                            }
                        }
                    }
                }

                let _ = receiver_thread.await;
            }
            Err(e) => {
                match e {
                    tokio_tungstenite::tungstenite::Error::Http(e) => {
                        if let Some(body) = e.body() {
                            println!("Error: {:?}", String::from_utf8(body.to_vec()));
                        } else {
                            println!("Http Error: {:?}", e);
                        }
                    }
                    _ => {
                        println!("Error: {:?}", e);
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn mine_and_send(
    address: &str,
    _mine_task: Arc<Mutex<MineTask>>,
    threads: u32,
    request_mine: Arc<Mutex<bool>>,
    last_challenge: Arc<Mutex<[u8; 32]>>,
    max_cutoff: u32,
) -> bool {
    let pubkey = Pubkey::from_str(address).expect("Invalid pubkey");
    let mine_task_share = Arc::clone(&_mine_task);
    let request_mine_clone = Arc::clone(&request_mine);
    let last_challenge_clone = Arc::clone(&last_challenge);
    let max_cutoff_clone = max_cutoff.clone();
    loop {
        let mut mine_task_clone = mine_task_share.lock().await;
        let mut mine_task = mine_task_clone.clone();
        mine_task_clone.sender = None;
        drop(mine_task_clone);
        let old_challenge = mine_task.challenge.clone();
        if mine_task.sender.is_none() {
            println!("No challenge to mine, waitting...");
            let mut last_challenge = last_challenge_clone.lock().await;
            *last_challenge = [0; 32];
            drop(last_challenge);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        // 如果mine_task的receive_time+cutoff超过当前时间，则重新发送StartMining消息
        if
            SystemTime::now().duration_since(mine_task.receive_time).unwrap().as_secs() >
            mine_task.cutoff
        {
            println!("Challenge expired, waitting...");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let nonce_range = mine_task.nonce_range.clone();
        let challenge = mine_task.challenge;
        let mut cutoff: u64 =
            mine_task.cutoff -
            SystemTime::now().duration_since(mine_task.receive_time).unwrap().as_secs();
        if cutoff > max_cutoff_clone as u64 {
            cutoff = max_cutoff_clone as u64;
        }
        println!("Mining starting... {}", mine_task.url);
        println!("Nonce range: {} - {}", nonce_range.start, nonce_range.end);
        println!("Cutoff: {}", cutoff);
        let mut request_mine_lock = request_mine_clone.lock().await;
        *request_mine_lock = false;
        let hash_timer = Instant::now();
        let core_ids = core_affinity::get_core_ids().unwrap();
        let nonces_per_thread = 10_000;
        let handles = core_ids
            .into_iter()
            .map(|i| {
                std::thread::spawn({
                    let mut memory = equix::SolverMemory::new();
                    move || {
                        if (i.id as u32).ge(&threads) {
                            return None;
                        }
                        let _ = core_affinity::set_for_current(i);
                        let first_nonce = nonce_range.start + nonces_per_thread * (i.id as u64);
                        let mut nonce = first_nonce;
                        let mut best_nonce = nonce;
                        let mut best_difficulty = 0;
                        let mut best_hash = drillx_2::Hash::default();
                        let mut total_hashes: u64 = 0;
                        loop {
                            // Create hash
                            for hx in drillx_2::get_hashes_with_memory(
                                &mut memory,
                                &challenge,
                                &nonce.to_le_bytes()
                            ) {
                                total_hashes += 1;
                                let difficulty = hx.difficulty();
                                if difficulty.gt(&best_difficulty) {
                                    best_nonce = nonce;
                                    best_difficulty = difficulty;
                                    best_hash = hx;
                                }
                            }

                            // Exit if processed nonce range
                            if nonce >= nonce_range.end {
                                break;
                            }

                            if nonce % 50 == 0 {
                                if hash_timer.elapsed().as_secs().ge(&cutoff) {
                                    break;
                                }
                            }

                            // Increment nonce
                            nonce += 1;
                        }

                        // Return the best nonce
                        Some((best_nonce, best_difficulty, best_hash, total_hashes))
                    }
                })
            })
            .collect::<Vec<_>>();

        // Join handles and return best nonce
        let mut best_nonce: u64 = 0;
        let mut best_difficulty = 0;
        let mut best_hash = drillx_2::Hash::default();
        let mut total_nonces_checked = 0;
        for h in handles {
            if let Ok(Some((nonce, difficulty, hash, nonces_checked))) = h.join() {
                total_nonces_checked += nonces_checked;
                if difficulty > best_difficulty {
                    best_difficulty = difficulty;
                    best_nonce = nonce;
                    best_hash = hash;
                }
            }
        }
        let hash_time = hash_timer.elapsed();

        println!("Found best diff: {}", best_difficulty);
        println!("Processed: {}", total_nonces_checked);
        println!("Hash time: {:?}", hash_time);
        if hash_time.as_secs() == 0 {
        } else {
            println!(
                "Hashpower: {:?} H/s",
                total_nonces_checked.saturating_div(hash_time.as_secs())
            );
        }
        let message_type = 2u8; // 1 u8 - BestSolution Message
        let best_hash_bin = best_hash.d; // 16 u8
        let best_nonce_bin = best_nonce.to_le_bytes(); // 8 u8

        let mut hash_nonce_message = [0; 24];
        hash_nonce_message[0..16].copy_from_slice(&best_hash_bin);
        hash_nonce_message[16..24].copy_from_slice(&best_nonce_bin);
        // let signature = key.sign_message(&hash_nonce_message).to_string().as_bytes().to_vec();

        let mut bin_data = [0; 57];
        bin_data[00..1].copy_from_slice(&message_type.to_le_bytes());
        bin_data[01..17].copy_from_slice(&best_hash_bin);
        bin_data[17..25].copy_from_slice(&best_nonce_bin);
        // bin_data[25..57].copy_from_slice(&key.pubkey().to_bytes());
        bin_data[25..57].copy_from_slice(&pubkey.to_bytes());

        let mut bin_vec = bin_data.to_vec();
        // bin_vec.extend(signature);
        bin_vec.extend("xxx".bytes());
        let message_sender = &mine_task.sender;
        let message_sender_clone = message_sender.clone().unwrap();
        let mut message_sender = message_sender_clone.lock().await;
        let _ = message_sender.send(Message::Binary(bin_vec)).await;
        drop(mine_task);
        println!("Mining complete!!!!!!!!!!!");
        *request_mine_lock = true;
        drop(request_mine_lock);
        let mut last_challenge = last_challenge_clone.lock().await;
        *last_challenge = challenge;
        drop(last_challenge);
        tokio::time::sleep(Duration::from_millis(800)).await;
    }
}
pub async fn mine(args: MineArgs, key: Keypair, url: String, unsecure: bool) {
    let _address = args.address;
    let threads = args.cores;
    let max_cutoff = args.cutoff;
    let mine_task = Arc::new(Mutex::new(MineTask::new()));
    // 将 url 按照逗号分隔成一个字符串向量
    let urls: Vec<String> = url
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();
    let _address_clone = _address.clone();
    let request_for_mine = Arc::new(Mutex::new(true));
    let last_challenge = Arc::new(Mutex::new([0; 32]));
    for _url in urls.into_iter() {
        let mine_task_clone = mine_task.clone();
        let _address_clone_clone = _address_clone.clone();
        let request_clone = request_for_mine.clone();
        let last_challenge_clone = last_challenge.clone();
        tokio::spawn(async move {
            connect_and_receive(
                &_address_clone_clone,
                _url.to_string(),
                mine_task_clone,
                request_clone,
                last_challenge_clone
            ).await;
        });
    }
    let mine_task_clone = mine_task.clone();
    let _address_clone = _address.clone();
    let request_clone = request_for_mine.clone();
    let last_challenge = last_challenge.clone();
    let max_cutoff_clone = max_cutoff.clone();
    tokio::spawn(async move {
        mine_and_send(
            &_address_clone,
            mine_task_clone,
            threads,
            request_clone,
            last_challenge,
            max_cutoff_clone,
        ).await;
    });
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn process_message(
    msg: Message,
    message_channel: UnboundedSender<ServerMessage>
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!("\n>>> Server Message: \n{}\n", t);
        }
        Message::Binary(b) => {
            let message_type = b[0];
            match message_type {
                0 => {
                    if b.len() < 49 {
                        println!("Invalid data for Message StartMining");
                    } else {
                        let mut hash_bytes = [0u8; 32];
                        // extract 256 bytes (32 u8's) from data for hash
                        let mut b_index = 1;
                        for i in 0..32 {
                            hash_bytes[i] = b[i + b_index];
                        }
                        b_index += 32;

                        // extract 64 bytes (8 u8's)
                        let mut cutoff_bytes = [0u8; 8];
                        for i in 0..8 {
                            cutoff_bytes[i] = b[i + b_index];
                        }
                        b_index += 8;
                        let cutoff = u64::from_le_bytes(cutoff_bytes);

                        let mut nonce_start_bytes = [0u8; 8];
                        for i in 0..8 {
                            nonce_start_bytes[i] = b[i + b_index];
                        }
                        b_index += 8;
                        let nonce_start = u64::from_le_bytes(nonce_start_bytes);

                        let mut nonce_end_bytes = [0u8; 8];
                        for i in 0..8 {
                            nonce_end_bytes[i] = b[i + b_index];
                        }
                        let nonce_end = u64::from_le_bytes(nonce_end_bytes);

                        let msg = ServerMessage::StartMining(
                            hash_bytes,
                            nonce_start..nonce_end,
                            cutoff
                        );

                        let _ = message_channel.send(msg);
                    }
                }
                _ => {
                    println!("Failed to parse server message type");
                }
            }
        }
        Message::Ping(v) => {
            println!("Got Ping: {:?}", v);
        }
        Message::Pong(v) => {
            println!("Got Pong: {:?}", v);
        }
        Message::Close(v) => {
            println!("Got Close: {:?}", v);
            return ControlFlow::Break(());
        }
        _ => {
            println!("Got invalid message data");
        }
    }

    ControlFlow::Continue(())
}
