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
use std::io::{Write, BufReader};
use std::time;
use toml::Value;
use std::fs::File;


const MTU: usize = 1500;
const EMPTY_PKT: [u8; MTU] = [0; MTU];
const SAFETY_BUFFER: f64 = 0.0;
const NUM_SEC_BW_UPDATES: f64 = 1.0;
// const NUM_PKTS_TO_SAVE: f64 = 1e5;

const IP_HEADER_LEN: usize = 20;
const MIN_PAYLOAD_LEN: usize = 9; // Seq number + flow
const VPN_HEADER_LEN: usize = 80;
const IP_SRC_ADDR_OFFSET: usize = 12;
const IP_DST_ADDR_OFFSET: usize = 16;
const IP_ADDR_LEN: usize = 4;
const IP_VERSION: u8 = 4;
const IP_ADDR_LISTEN_TO: [u8;4] = [10,7,0,4];

const AVG_CAIDA_LEN: f64 = 900.0;
const FACTOR_MEGABITS: f64 = 1e6;
const BITS_PER_BYTE: f64 = 8.0;
const WRAP_AND_WIREGUARD_OVERHAD: f64 = 100.0;

struct ChannelCustom {
    tx: Box<dyn datalink::DataLinkSender>,
    rx: Box<dyn datalink::DataLinkReceiver>,
}

pub fn run(settings: Value) -> Result<(), Box<dyn Error>> {
    // Channel to communicate data on it
    let (sender, receiver) = mpsc::channel();

    // Flow is 0 if none specified
    let flow = match get_env_var_f64("FLOW") {
        Ok(f) => f as u8,
        Err(_) => 0_u8,
    };

    let rate = settings["general"]["rate"].as_float().expect("Rate setting not found");
    let sending_time = settings["general"]["time"].as_float().expect("Sending time setting not found");
    let save_data = settings["general"]["save"].as_bool().expect("Save setting not found");
    let is_sender = settings["general"]["send"].as_bool().expect("Send setting not found");
    let is_receiver = settings["general"]["receive"].as_bool().expect("Receive setting not found");
    let is_log = settings["general"]["log"].as_bool().expect("Is log setting not found");
    let dataset = settings["general"]["dataset"].as_str().expect("Dataset setting not found").to_string();

    let mut avg_len = 0.0;
    if dataset == "" {
        // Uniform
        let max_len = (MTU - IP_HEADER_LEN - VPN_HEADER_LEN) as f64;
        let min_len = (IP_HEADER_LEN + MIN_PAYLOAD_LEN) as f64;
        avg_len = (max_len + min_len) / 2.0 + WRAP_AND_WIREGUARD_OVERHAD;  
    } else if dataset == "caida" {
        avg_len = AVG_CAIDA_LEN + WRAP_AND_WIREGUARD_OVERHAD;
    }

    let pps = rate / avg_len * FACTOR_MEGABITS / BITS_PER_BYTE;
    let num_pkts = (pps * sending_time) as usize;

    let ip_src = parse_ip(settings["ip"]["src"].as_str().expect("Src ip address not found").to_string());
    let ip_dst = parse_ip(settings["ip"]["dst"].as_str().expect("Dst ip address not found").to_string());
    
    let is_send_isolated = settings["isolation"]["isolate_send"].as_bool().expect("Isolate send setting not found");  
    let core_id_send = settings["isolation"]["core_send"].as_integer().expect("Core send setting not found") as usize;

    let is_receive_isolated = settings["isolation"]["isolate_receive"].as_bool().expect("Isolate receive setting not found");     
    let core_id_receive = settings["isolation"]["core_receive"].as_integer().expect("Core receive setting not found") as usize;

    let priority = settings["isolation"]["priority"].as_integer().expect("Thread priority setting not found") as i32; 

    let input = settings["interface"]["input"].as_str().expect("Input interface setting not found").to_string(); 
    let output = settings["interface"]["output"].as_str().expect("Output interface setting not found").to_string(); 

    if is_log {
        println!("Sending Ethernet frames on interface {}...", input);
        println!("Receiving Ethernet frames on interface {}...", output);
        println!("Sending on specific cores = {}", is_send_isolated);    
    }
    
    if is_receiver {
        // Spawn thread for receiving packets
        let recv_handle = thread::spawn(move || {
        if is_receive_isolated {
            unsafe {
                let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
                libc::CPU_SET(core_id_receive, &mut cpuset);
                libc::sched_setaffinity(0, std::mem::size_of_val(&cpuset), &cpuset);

                let thread =  libc::pthread_self();
                let param = libc::sched_param { sched_priority: priority };
                let result = libc::pthread_setschedparam(thread, libc::SCHED_FIFO, &param as *const libc::sched_param);
                if result != 0 {
                    panic!("Failed to set thread priority");
                }
            }
        }
            receive(&output, receiver, pps, num_pkts, save_data);
        });

        recv_handle.join().expect("Receiving thread panicked");
    }

    if is_sender {
        // Spawn thread for sending packets
        let send_handle = thread::spawn(move || {
            if is_send_isolated {
                unsafe {
                    let mut cpuset: libc::cpu_set_t = std::mem::zeroed();
                    libc::CPU_SET(core_id_send, &mut cpuset);
                    libc::sched_setaffinity(0, std::mem::size_of_val(&cpuset), &cpuset);

                    let thread =  libc::pthread_self();
                    let param = libc::sched_param { sched_priority: priority };
                    let result = libc::pthread_setschedparam(thread, libc::SCHED_FIFO, &param as *const libc::sched_param);
                    if result != 0 {
                        panic!("Failed to set thread priority");
                    }
                }
            }

            send(&input, sender, pps, ip_src, ip_dst, num_pkts, save_data, flow, dataset);
        });
        // Wait 1s before tsarting to send
        thread::sleep(Duration::new(1, 0));
        send_handle.join().expect("Sending thread panicked");
    }

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

fn send(input: &str, sender: Sender<i64>, pps: f64, ip_src: [u8;4], ip_dst: [u8;4], num_pkts: usize, save_data: bool, flow: u8, dataset: String) {
    let mut ch_tx = match get_channel(input) {
        Ok(tx) => tx,
        Err(error) => panic!("Error getting channel: {error}"),
    };

    let mut file = OpenOptions::new()
        .write(true)
        .truncate(save_data) // Overwrite
        .create(true)
        .open(format!("tx_data_{}.csv", flow))
        .expect("Could not open file");

    if save_data {
        writeln!(file, "Seq,Time,Flow").expect("Failed to write to file");
    }

    //let mut packets = get_eth_frames();
    //let packets = get_perfect_frames(vec![64,751]);
    let mut count: usize = 0;
    let delays = vec![0; 1e6 as usize];

    //let interval = Duration::from_micros((1e6/pps) as u64);
    let interval = Duration::from_nanos((1e9/pps + SAFETY_BUFFER) as u64);
    //let interval = Duration::from_nanos(SLEEP_TIME);
    let mut last_iteration_time = Instant::now();
    // let mut last_msg_time = Instant::now();

    let lengths;
    if dataset == "caida" {
        lengths = get_caida_lengths(num_pkts);
    } else {
        lengths = get_random_pkt_lengths(num_pkts);
    }

    println!("Sending...");
    while count < num_pkts {
    //loop {
        let frame = &mut get_ipv4_packet(ip_src, ip_dst, flow, lengths[count] as usize);
        // println!("{:?}", frame);
        encode_sequence_num(  frame, count);
        // println!("{}", frame.len());
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

        if save_data && count < delays.len() {
            // Move this to the end if too unefficient
            let current_time = time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap();
            writeln!(file, "{},{},{}", count, current_time.as_nanos(), flow).expect("Failed to write to file");
            //delays[count as usize] = elapsed_time.as_nanos()
        }

        if count % (pps * NUM_SEC_BW_UPDATES) as usize == 0 && count < num_pkts {
            // let seq = decode_sequence_num(frame);
            // println!("Sending {} of length {}", seq, frame.len());
            sender.send(count as i64).unwrap();
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

fn receive(output: &str, receiver: Receiver<i64>, pps: f64, num_pkts: usize, save_data: bool) {
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
        writeln!(file, "Seq,Time,Flow").expect("Failed to write to file");
    }

    // let mut total_seq_mismatch = 0;
    let mut max_seq_mismatch = 0;

    let mut last_msg_time = Instant::now();
    let mut count: usize = 0;

    println!("Receiving {}pkt...", num_pkts);
    while count < num_pkts as usize {
    // loop {
        match ch_rx.rx.next() {
            // process_packet(packet, &mut scheduler),
            Ok(pkt) =>  {
                if is_ip_addr_matching(pkt, false) { 
                    let seq = decode_sequence_num(pkt);
                    let mismatch = seq.abs_diff(count);
                    // println!("{seq}");
                    // total_seq_mismatch += mismatch;
                    if mismatch > max_seq_mismatch {
                        max_seq_mismatch = mismatch;
                    } 

                    if save_data {
                        // Move this to the end if too unefficient
                        let rx_time = time::SystemTime::now().duration_since(time::UNIX_EPOCH).unwrap().as_nanos();
                        let rx_seq = seq;
                        let rx_flow = pkt[pkt.len()-1];
                        writeln!(file, "{},{},{}", rx_seq, rx_time, rx_flow).expect("Failed to write to file");
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
                let avg_reorder = 0;
                if count > 0 {
                    // avg_reorder = total_seq_mismatch/count;
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

fn get_ipv4_packet( ip_src: [u8;4], ip_dst: [u8;4], flow: u8, pkt_len: usize) -> Vec<u8> {
    let mut ip_buff = EMPTY_PKT[0..pkt_len].to_vec();
    let mut packet = ipv4::MutableIpv4Packet::new(&mut ip_buff).unwrap();

    // Set the IP header fields
    packet.set_version(IP_VERSION);
    packet.set_header_length((IP_HEADER_LEN/4) as u8);
    packet.set_total_length(pkt_len as u16); // Set the total length of the packet
    //packet.set_identification(1234);
    packet.set_ttl(64);
    packet.set_next_level_protocol(IpNextHeaderProtocols::Udp); 
    packet.set_source(ip_src.into());
    packet.set_destination(ip_dst.into());

    packet.set_checksum(pnet::packet::ipv4::checksum(&packet.to_immutable()));

    // Encode flow in last byte
    ip_buff[pkt_len-1] = flow;

    ip_buff
}

fn get_random_pkt_lengths(num_pkts: usize) -> Vec<i32> {
    let mut rng = rand::thread_rng();
    let mut pkt_lengths = Vec::with_capacity(num_pkts);

    for _ in 0..num_pkts {
        pkt_lengths.push(rng.gen_range((IP_HEADER_LEN+MIN_PAYLOAD_LEN) as i32..=(MTU-IP_HEADER_LEN-VPN_HEADER_LEN) as i32));
    }
    
    pkt_lengths
}

fn get_caida_lengths(num_pkts: usize) -> Vec<i32> {
    let filename = "../analysis/caida_lengths_small.csv";

    let file = File::open(filename).expect("Error opening caida length file");
    let reader = BufReader::new(file);

    let mut pkt_lengths = Vec::with_capacity(num_pkts);

    let mut csv_reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(reader);

    let mut count = 0;
    for result in csv_reader.records() {
        let record = result.expect("Could not read line in file");
        if count >= num_pkts {
            break; 
        }

        if let Some(field) = record.get(0) {
            if let Ok(length) = field.parse::<i32>() {
                if length > (MTU-IP_HEADER_LEN-VPN_HEADER_LEN) as i32 {
                    // Simulate fragmentation (if MTU is less than half would need to handle more than 2 fragments)
                    pkt_lengths.push((MTU-IP_HEADER_LEN-VPN_HEADER_LEN) as i32);
                    // Length - previous fragment + additional ip header
                    // let second_fragment_len = length - (MTU-IP_HEADER_LEN-VPN_HEADER_LEN) as i32 + IP_HEADER_LEN as i32;
                    // pkt_lengths.push(second_fragment_len);
                    count += 1;
                } else if length > (IP_HEADER_LEN + MIN_PAYLOAD_LEN) as i32 {
                    pkt_lengths.push(length);
                    count += 1;
                }
            } else {
                println!("Error: Failed to parse i32");
            }
        } else {
            println!("Error: Missing csv field");
        }
    }

    pkt_lengths
}

fn encode_sequence_num(arr: &mut Vec<u8>, seq: usize) {
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

fn is_ip_addr_matching(buff: &[u8], is_dest: bool) -> bool {
    // let pkt = ipv4::Ipv4Packet::new(buff).unwrap();
    // let dst_addr = pkt.get_destination();
    // println!("{}, {}", dst_addr, net::Ipv4Addr::new(DST_IP_ADDR[0], DST_IP_ADDR[1], DST_IP_ADDR[2], DST_IP_ADDR[3]));
    // dst_addr == net::Ipv4Addr::new(DST_IP_ADDR[0], DST_IP_ADDR[1], DST_IP_ADDR[2], DST_IP_ADDR[3])
    if is_dest {
        buff[IP_DST_ADDR_OFFSET..IP_DST_ADDR_OFFSET+IP_ADDR_LEN] == IP_ADDR_LISTEN_TO
    } else {
        buff[IP_SRC_ADDR_OFFSET..IP_SRC_ADDR_OFFSET+IP_ADDR_LEN] == IP_ADDR_LISTEN_TO
    }
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

fn parse_ip(ip_str: String) -> [u8;4] {
    let ip_addr = match ip_str.parse::<net::Ipv4Addr>() {
        Ok(addr) => addr,
        Err(e) => {
            panic!("Failed to parse IP address: {}", e);
        }
    };
    ip_addr.octets()
}
