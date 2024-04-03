use std::sync::mpsc::{self, Receiver, Sender};
use pnet::datalink;
use std::error::Error;
use std::thread;
use pnet::packet::ethernet;
use pnet::util::MacAddr;
use rand::prelude::*;
use std::time::{Duration, Instant};

const NUM_PACKETS: f64 = 1e6;
const MIN_ETH_LEN: i32 = 64;
const MTU: usize = 1500;
const EMPTY_PKT: [u8; MTU] = [0; MTU];
const PACKETS_PER_SECOND: f64 = 0.8e4;
const SAFETY_BUFFER: f64 = 0.0;
const NUM_SEC_BW_UPDATES: f64 = 1.0;
const ETH_HDR_LEN: usize = 14;
// Time in ns
const SLEEP_TIME: u64 = 4000;  //4500 no queues but slow

struct ChannelCustom {
    tx: Box<dyn datalink::DataLinkSender>,
    rx: Box<dyn datalink::DataLinkReceiver>,
}

pub fn run(input: String, output: String) -> Result<(), Box<dyn Error>> {
    println!("Sending Ethernet frames on interface {}...", input);
    println!("Receiving Ethernet frames on interface {}...", output);

    let (sender, receiver) = mpsc::channel();

    let is_run_specific_core = false;
    let core_id_tx = 0;
    let core_id_rx = 1;

    println!("Sending on specific cores = {}", is_run_specific_core);

    // Spawn thread for sending packets
    let send_handle = thread::spawn(move || {
        if is_run_specific_core {
            unsafe {
                let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_SET(core_id_tx, &mut cpuset);
                libc::sched_setaffinity(0, std::mem::size_of_val(&cpuset), &cpuset);
            }
        }

        send(&input, sender);
    });

    // Spawn thread for receiving packets
    let recv_handle = thread::spawn(move || {
        if is_run_specific_core {
            unsafe {
                let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_SET(core_id_rx, &mut cpuset);
                libc::sched_setaffinity(0, std::mem::size_of_val(&cpuset), &cpuset);
            }
        }

        receive(&output, receiver);
    });

    send_handle.join().expect("Sending thread panicked");
    recv_handle.join().expect("Receiving thread panicked");

    Ok(())
}

fn get_channel(interface_name: &str) -> Result<ChannelCustom, &'static str>{
    // Retrieve the network interface
    let interfaces = datalink::interfaces();
    let interface = match interfaces
        .into_iter()
        .find(|iface| iface.name == interface_name) {
            Some(inter) => inter,
            None => return Err("Failed to find network interface"),
        };

    // Create a channel to receive Ethernet frames
    let (tx, rx) = match datalink::channel(&interface, Default::default()) {
        Ok(datalink::Channel::Ethernet(tx, rx)) => (tx, rx),
        Ok(_) => return Err("Unknown channel type"),
        Err(e) => panic!("Failed to create channel {e}"),
    };

    let ch = ChannelCustom{ 
        tx, 
        rx,
    };

    Ok(ch)
}

fn send(input: &str, sender: Sender<i64>) {
    let mut ch_tx = match get_channel(input) {
        Ok(tx) => tx,
        Err(error) => panic!("Error getting channel: {error}"),
    };

    let mut packets = get_eth_frames();
    //let packets = get_perfect_frames(vec![64,751]);
    let mut i = 0;
    let mut count = 0;

    //let interval = Duration::from_micros((1e6/PACKETS_PER_SECOND) as u64);
    let interval = Duration::from_nanos((1e9/PACKETS_PER_SECOND + SAFETY_BUFFER) as u64);
    //let interval = Duration::from_nanos(SLEEP_TIME);
    let mut last_iteration_time = Instant::now();
    let mut last_msg_time = Instant::now();

    loop {
        let frame = &mut packets[i];
        encode_sequence_num(  frame, count);
        match ch_tx.tx.send_to(frame, None) {
            Some(res) => {
                match res {
                    Ok(_) => (),
                    Err(e) => eprintln!("Error sending frame: {}", e),
                }
            }
            None => {
                eprintln!("No packets to send");
            }
        }
        i = (i+1) % NUM_PACKETS as usize;
        if count % (PACKETS_PER_SECOND * NUM_SEC_BW_UPDATES) as i64 == 0 {
            sender.send(count).unwrap();
            //println!("Sent {} packets in {:?}", count, last_msg_time.elapsed()); 
            last_msg_time = Instant::now();
        }
        count += 1;

        // Calculate time to sleep
        let elapsed_time = last_iteration_time.elapsed();
        last_iteration_time = Instant::now();
        let sleep_time = if elapsed_time < interval {
            interval - elapsed_time
        } else {
            Duration::new(0, 0)
        };
        // Sleep for the remaining time until the next iteration
        thread::sleep(sleep_time);
    }
}

fn receive(output: &str, receiver: Receiver<i64>) {
    let mut ch_rx = match get_channel(output) {
        Ok(rx) => rx,
        Err(error) => panic!("Error getting channel: {error}"),
    };

    let mut total_seq_mismatch = 0;
    let mut max_seq_mismatch = 0;
    let mut min_seq_mismatch = 0;

    let mut last_msg_time = Instant::now();
    let mut count = 0;
    loop {
        match ch_rx.rx.next() {
            // process_packet(packet, &mut scheduler),
            Ok(pkt) =>  {
                let seq = decode_sequence_num(pkt);
                total_seq_mismatch += seq - count;
                if seq - count > max_seq_mismatch {
                    max_seq_mismatch = seq - count;
                } else if seq - count < min_seq_mismatch {
                    min_seq_mismatch = seq - count;
                }
                count += 1;
            },
            Err(e) => {
                eprintln!("Error receiving frame: {}", e);
                continue;
            }
        };

        if count % PACKETS_PER_SECOND as i64 == 0 {
            //println!("Received {} in {:?}", count, last_msg_time.elapsed());
            last_msg_time = Instant::now();
        }

        match receiver.try_recv() {
            Ok(num_pkts) => {
                let latency_ratio = (num_pkts as f64 - count as f64) / num_pkts as f64;
                let latency_time = last_msg_time.elapsed().as_nanos() as f64 * (latency_ratio / (PACKETS_PER_SECOND * NUM_SEC_BW_UPDATES));
                let latency_total = 1e3 * (num_pkts as f64 - count as f64) / (PACKETS_PER_SECOND * NUM_SEC_BW_UPDATES);
                last_msg_time = Instant::now();
                println!("Latency in received packets of {:.2}% or {:.0}ns, total {:.4}ms", 100.0 * latency_ratio, latency_time, latency_total);
                println!("Average reordering of {} packets, max of {} and min of {}", total_seq_mismatch/count, max_seq_mismatch, min_seq_mismatch);
            },
            _ => continue,
        }
        
    }
}

fn get_eth_frames() -> Vec<Vec<u8>>{
    let src_mac = MacAddr::new(0x05, 0x04, 0x03, 0x02, 0x01, 0x00);
    let dst_mac = MacAddr::new(0x00, 0x01, 0x02, 0x03, 0x04, 0x05);
    let mut frame_buff: Vec<Vec<u8>> = Vec::new();
    for _ in 0..NUM_PACKETS as i32 {
        let length = get_random_pkt_len() as usize;
        let mut eth_buff = EMPTY_PKT[0..length].to_vec();
        let mut eth_pkt = ethernet::MutableEthernetPacket::new(&mut eth_buff).unwrap();
        eth_pkt.set_source(src_mac);
        eth_pkt.set_destination(dst_mac);
        eth_pkt.set_ethertype(ethernet::EtherType::new(length as u16));

        frame_buff.push(eth_buff);
    }
    //println!("{:?}", frame_buff[0]);
    frame_buff
}

fn get_perfect_frames(pattern: Vec<u16>) -> Vec<Vec<u8>>{
    let src_mac = MacAddr::new(0x05, 0x04, 0x03, 0x02, 0x01, 0x00);
    let dst_mac = MacAddr::new(0x00, 0x01, 0x02, 0x03, 0x04, 0x05);
    let mut frame_buff: Vec<Vec<u8>> = Vec::new();
    for i in 0..NUM_PACKETS as usize {
        let length = pattern[i % pattern.len()] as usize;
        let mut eth_buff = EMPTY_PKT[0..length].to_vec();
        let mut eth_pkt = ethernet::MutableEthernetPacket::new(&mut eth_buff).unwrap();
        eth_pkt.set_source(src_mac);
        eth_pkt.set_destination(dst_mac);
        eth_pkt.set_ethertype(ethernet::EtherType::new(length as u16));

        //frame_buff.push(eth_buff.clone());
        frame_buff.push(eth_buff);
    }
    //println!("{:?}", frame_buff[0]);
    frame_buff
}

fn get_random_pkt_len() -> i32 {
    let mut rng = rand::thread_rng();
    rng.gen_range(MIN_ETH_LEN..=MTU as i32)
}

fn encode_sequence_num(arr: &mut Vec<u8>, seq: i64) {
    // Encode as 32bit integer -> 4 bytes
    arr[ETH_HDR_LEN] = ((seq >> 56) & 0xFF) as u8;
    arr[ETH_HDR_LEN+1] = ((seq >> 48) & 0xFF) as u8;
    arr[ETH_HDR_LEN+2] = ((seq >> 40) & 0xFF) as u8;
    arr[ETH_HDR_LEN+3] = ((seq >> 32) & 0xFF) as u8;
    arr[ETH_HDR_LEN+4] = ((seq >> 24) & 0xFF) as u8;
    arr[ETH_HDR_LEN+5] = ((seq >> 16) & 0xFF) as u8;
    arr[ETH_HDR_LEN+6] = ((seq >> 8) & 0xFF) as u8;
    arr[ETH_HDR_LEN+7] = (seq & 0xFF) as u8;
}

fn decode_sequence_num(arr: &[u8]) -> i64 {
    // Encode as 32bit integer -> 4 bytes24
    let seq = (arr[ETH_HDR_LEN] as i64) << 56 |
                    (arr[ETH_HDR_LEN+1] as i64) << 48 |
                    (arr[ETH_HDR_LEN+2] as i64) << 40 |
                    (arr[ETH_HDR_LEN+3] as i64) << 32 |
                    (arr[ETH_HDR_LEN+4] as i64) << 24 |
                    (arr[ETH_HDR_LEN+5] as i64) << 16 |
                    (arr[ETH_HDR_LEN+6] as i64) << 8 |
                    arr[ETH_HDR_LEN+7] as i64;
    
    seq
}