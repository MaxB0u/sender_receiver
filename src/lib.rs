use std::net;
use std::sync::mpsc::{self, Receiver, Sender};
use pnet::datalink;
use pnet::packet::ip::IpNextHeaderProtocols;
use pnet::packet::ipv4;
use std::error::Error;
use std::thread;
// use pnet::packet::ethernet;
// use pnet::util::MacAddr;
use rand::prelude::*;
use std::time::{Duration, Instant};
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::time;

// const NUM_PACKETS: f64 = 1e1;
const MIN_ETH_LEN: i32 = 64;
const MTU: usize = 1500;
const IP_HEADER_LEN: usize = 20;
const VPN_HEADER_LEN: usize = 80;
const EMPTY_PKT: [u8; MTU] = [0; MTU];
//const PACKETS_PER_SECOND: f64 = 0.8e4;
const SAFETY_BUFFER: f64 = 0.0;
const NUM_SEC_BW_UPDATES: f64 = 1.0;
// const ETH_HDR_LEN: usize = 14;
const NUM_PKTS_TO_SAVE: f64 = 1e5;

const IP_VERSION: u8 = 4;
// const SRC_IP_ADDR: [u8;4] = [10, 10, 0, 1];
const DST_IP_ADDR: [u8;4] = [10, 10, 1, 1];

struct ChannelCustom {
    tx: Box<dyn datalink::DataLinkSender>,
    rx: Box<dyn datalink::DataLinkReceiver>,
}

pub fn run(input: String, output: String, pps: f64, flow: u8) -> Result<(), Box<dyn Error>> {
    println!("Sending Ethernet frames on interface {}...", input);
    println!("Receiving Ethernet frames on interface {}...", output);
    let save_data = true;

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

        send(&input, sender, pps, flow, save_data);
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

        receive(&output, receiver, pps, flow, save_data);
    });

    recv_handle.join().expect("Receiving thread panicked");
    // Wait 1s before tsarting to send
    thread::sleep(Duration::new(1, 0));
    send_handle.join().expect("Sending thread panicked");

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

fn send(input: &str, sender: Sender<i64>, pps: f64, flow: u8, save_data: bool) {
    let mut ch_tx = match get_channel(input) {
        Ok(tx) => tx,
        Err(error) => panic!("Error getting channel: {error}"),
    };

    let mut file = OpenOptions::new()
        .write(true)
        .truncate(save_data) // Overwrite
        .create(true)
        .open("tx_data.csv")
        .expect("Could not open file");

    if save_data {
        writeln!(file, "Seq,Time").expect("Failed to write to file");
    }

    //let mut packets = get_eth_frames();
    //let packets = get_perfect_frames(vec![64,751]);
    let mut count = 0;
    let delays = vec![0; 1e6 as usize];

    //let interval = Duration::from_micros((1e6/pps) as u64);
    let interval = Duration::from_nanos((1e9/pps + SAFETY_BUFFER) as u64);
    //let interval = Duration::from_nanos(SLEEP_TIME);
    let mut last_iteration_time = Instant::now();
    // let mut last_msg_time = Instant::now();

    while count < NUM_PKTS_TO_SAVE as i64 {
    //for _ in (0..NUM_PACKETS as usize) {
        let frame = &mut get_ipv4_packet(flow);
        // println!("{:?}", frame);
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

        if save_data && count < delays.len() as i64 {
            // Move this to the end if too unefficient
            let current_time = time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap();
            writeln!(file, "{},{}", count, current_time.as_nanos()).expect("Failed to write to file");
            //delays[count as usize] = elapsed_time.as_nanos()
        }

        if count % (pps * NUM_SEC_BW_UPDATES) as i64 == 0 && count < NUM_PKTS_TO_SAVE as i64 {
            // let seq = decode_sequence_num(frame);
            // println!("Sending {} of length {}", seq, frame.len());
            sender.send(count).unwrap();
            //println!("Sent {} packets in {:?}", count, last_msg_time.elapsed()); 
            // last_msg_time = Instant::now();
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

fn receive(output: &str, receiver: Receiver<i64>, pps: f64, flow: u8, save_data: bool) {
    let mut ch_rx = match get_channel(output) {
        Ok(rx) => rx,
        Err(error) => panic!("Error getting channel: {error}"),
    };

    let mut file = OpenOptions::new()
        .write(true)
        .truncate(save_data) // Overwrite
        .create(true)
        .open("rx_data.csv")
        .expect("Could not open file");

    if save_data {
        writeln!(file, "Seq,Time").expect("Failed to write to file");
    }

    let mut total_seq_mismatch = 0;
    let mut max_seq_mismatch = 0;

    let mut last_msg_time = Instant::now();
    let mut count: usize = 0;
    let mut delays = vec![0; NUM_PKTS_TO_SAVE as usize];
    let mut seqs = vec![0; NUM_PKTS_TO_SAVE as usize];

    while count < NUM_PKTS_TO_SAVE as usize {
    // loop {
        match ch_rx.rx.next() {
            // process_packet(packet, &mut scheduler),
            Ok(pkt) =>  {
                if is_dst_ip_addr_matching(pkt, flow) { 
                    let seq = decode_sequence_num(pkt);
                    let mismatch = seq.abs_diff(count);
                    // println!("{seq}");
                    total_seq_mismatch += mismatch;
                    if mismatch > max_seq_mismatch {
                        max_seq_mismatch = mismatch;
                    } 

                    if save_data {
                        // Move this to the end if too unefficient
                        delays[count] = time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap().as_nanos();
                        seqs[count] = seq;
                        writeln!(file, "{},{}", seqs[count], delays[count]).expect("Failed to write to file");
                        //delays[count as usize] = elapsed_time.as_nanos()
                    }
                    count += 1;
                }
               
            },
            Err(e) => {
                eprintln!("Error receiving frame: {}", e);
                continue;
            }
        };

        // if count % pps as usize == 0 {
        //     println!("Received {} in {:?}", count, last_msg_time.elapsed());
        //     last_msg_time = Instant::now();
        // }

        match receiver.try_recv() {
            Ok(num_pkts) => {
                let latency_ratio = (num_pkts as f64 - count as f64) / num_pkts as f64;
                let latency_time = last_msg_time.elapsed().as_nanos() as f64 * (latency_ratio / (pps * NUM_SEC_BW_UPDATES));
                let latency_total = 1e3 * (num_pkts as f64 - count as f64) / (pps * NUM_SEC_BW_UPDATES);
                let mut avg_reorder = 0;
                if count > 0 {
                    avg_reorder = total_seq_mismatch/count;
                }

                last_msg_time = Instant::now();
                println!("Latency in received packets of {:.2}% or {:.0}ns, total {:.4}ms", 100.0 * latency_ratio, latency_time, latency_total);
                println!("Average reordering of {}, max of {}, received {} packets", avg_reorder, max_seq_mismatch, count);
            },
            _ => continue,
        }
        
    }

    // for i in 0..NUM_PKTS_TO_SAVE as usize {
    //     writeln!(file, "{},{}", seqs[i], delays[i]).expect("Failed to write to file");
    // }
}

// fn get_eth_frame(flow: u8) -> Vec<u8> {
//     let dst_mac = MacAddr::new(0x00, 0x01, 0x02, 0x03, 0x04, flow);
//     let src_mac = MacAddr::new(0x05, 0x04, 0x03, 0x02, 0x01, 0x00);
//     let length = get_random_pkt_len() as usize;
//     let mut eth_buff = EMPTY_PKT[0..length].to_vec();
//     let mut eth_pkt = ethernet::MutableEthernetPacket::new(&mut eth_buff).unwrap();
//     eth_pkt.set_source(src_mac);
//     eth_pkt.set_destination(dst_mac);
//     eth_pkt.set_ethertype(ethernet::EtherType::new(length as u16));

//     eth_buff
// }

fn get_ipv4_packet(flow: u8) -> Vec<u8> {
    let src_ip_addr = [10, 10, 0, flow];

    let length = get_random_pkt_len() as usize;
    let mut ip_buff = EMPTY_PKT[0..length].to_vec();
    let mut packet = ipv4::MutableIpv4Packet::new(&mut ip_buff).unwrap();

    // Set the IP header fields
    packet.set_version(IP_VERSION);
    packet.set_header_length((IP_HEADER_LEN/4) as u8);
    packet.set_total_length(length as u16); // Set the total length of the packet
    //packet.set_identification(1234);
    packet.set_ttl(64);
    packet.set_next_level_protocol(IpNextHeaderProtocols::Udp); 
    packet.set_source(src_ip_addr.into());
    packet.set_destination(DST_IP_ADDR.into());

    packet.set_checksum(pnet::packet::ipv4::checksum(&packet.to_immutable()));

    ip_buff
}

// fn get_eth_frames(flow: u8) -> Vec<Vec<u8>> {
//     let dst_mac = MacAddr::new(0x00, 0x01, 0x02, 0x03, 0x04, flow);
//     let src_mac = MacAddr::new(0x05, 0x04, 0x03, 0x02, 0x01, 0x00);
//     let mut frame_buff: Vec<Vec<u8>> = Vec::new();
//     for _ in 0..NUM_PACKETS as i32 {
//         let length = get_random_pkt_len() as usize;
//         let mut eth_buff = EMPTY_PKT[0..length].to_vec();
//         let mut eth_pkt = ethernet::MutableEthernetPacket::new(&mut eth_buff).unwrap();
//         eth_pkt.set_source(src_mac);
//         eth_pkt.set_destination(dst_mac);
//         eth_pkt.set_ethertype(ethernet::EtherType::new(length as u16));

//         frame_buff.push(eth_buff);
//     }
//     //println!("{:?}", frame_buff[0]);
//     frame_buff
// }

// fn get_perfect_frames(pattern: Vec<u16>) -> Vec<Vec<u8>>{
//     let dst_mac = MacAddr::new(0x00, 0x01, 0x02, 0x03, 0x04, 0x05);
//     let src_mac = MacAddr::new(0x05, 0x04, 0x03, 0x02, 0x01, 0x00);
//     let mut frame_buff: Vec<Vec<u8>> = Vec::new();
//     for i in 0..NUM_PACKETS as usize {
//         let length = pattern[i % pattern.len()] as usize;
//         let mut eth_buff = EMPTY_PKT[0..length].to_vec();
//         let mut eth_pkt = ethernet::MutableEthernetPacket::new(&mut eth_buff).unwrap();
//         eth_pkt.set_source(src_mac);
//         eth_pkt.set_destination(dst_mac);
//         eth_pkt.set_ethertype(ethernet::EtherType::new(length as u16));

//         //frame_buff.push(eth_buff.clone());
//         frame_buff.push(eth_buff);
//     }
//     //println!("{:?}", frame_buff[0]);
//     frame_buff
// }

fn get_random_pkt_len() -> i32 {
    let mut rng = rand::thread_rng();
    rng.gen_range(MIN_ETH_LEN..=(MTU-IP_HEADER_LEN-VPN_HEADER_LEN) as i32)
}

fn encode_sequence_num(arr: &mut Vec<u8>, seq: i64) {
    // Encode as 32bit integer -> 4 bytes
    arr[IP_HEADER_LEN] = ((seq >> 56) & 0xFF) as u8;
    arr[IP_HEADER_LEN+1] = ((seq >> 48) & 0xFF) as u8;
    arr[IP_HEADER_LEN+2] = ((seq >> 40) & 0xFF) as u8;
    arr[IP_HEADER_LEN+3] = ((seq >> 32) & 0xFF) as u8;
    arr[IP_HEADER_LEN+4] = ((seq >> 24) & 0xFF) as u8;
    arr[IP_HEADER_LEN+5] = ((seq >> 16) & 0xFF) as u8;
    arr[IP_HEADER_LEN+6] = ((seq >> 8) & 0xFF) as u8;
    arr[IP_HEADER_LEN+7] = (seq & 0xFF) as u8;
}

fn decode_sequence_num(arr: &[u8]) -> usize {
    // Encode as 32bit integer -> 4 bytes24
    let seq = (arr[IP_HEADER_LEN] as i64) << 56 |
                    (arr[IP_HEADER_LEN+1] as i64) << 48 |
                    (arr[IP_HEADER_LEN+2] as i64) << 40 |
                    (arr[IP_HEADER_LEN+3] as i64) << 32 |
                    (arr[IP_HEADER_LEN+4] as i64) << 24 |
                    (arr[IP_HEADER_LEN+5] as i64) << 16 |
                    (arr[IP_HEADER_LEN+6] as i64) << 8 |
                    arr[IP_HEADER_LEN+7] as i64;
    seq as usize
}

// fn is_dst_addr_matching(buff: &[u8], flow: u8) -> bool {
//     let expected_dst_mac = MacAddr::new(0x00, 0x01, 0x02, 0x03, 0x04, flow);
//     let eth_pkt = ethernet::EthernetPacket::new(buff).unwrap();
//     let dst_addr = eth_pkt.get_destination();

//     dst_addr == expected_dst_mac
// }

fn is_dst_ip_addr_matching(buff: &[u8], flow: u8) -> bool {
    let pkt = ipv4::Ipv4Packet::new(buff).unwrap();
    let dst_addr = pkt.get_destination();

    // println!("{}, {}", dst_addr, net::Ipv4Addr::new(DST_IP_ADDR[0], DST_IP_ADDR[1], DST_IP_ADDR[2], DST_IP_ADDR[3]));

    dst_addr == net::Ipv4Addr::new(DST_IP_ADDR[0], DST_IP_ADDR[1], DST_IP_ADDR[2], DST_IP_ADDR[3])
}

pub fn get_env_var_f64(name: &str) -> Result<f64, &'static str> {
    let var = match env::var(name) {
        Ok(var) => {
            match var.parse::<f64>() {
                Ok(var) => {
                    var
                },
                Err(_) => {
                    return Err("Error parsing env variable string");
                }
            }
        },
        Err(_) => {
            return Err("Error getting env vairable");
        },
    };
    Ok(var)
}