fn main() {
    // Get the name of the network interface from the command-line arguments
    let mut args: Vec<String> = std::env::args().collect();

    // Check if at least four arguments are provided
    if args.len() < 3 {
        eprintln!("Usage (give 2 interface names): {} <input> <output>", args[0]);
        std::process::exit(1);
    }

    let pps = sender_receiver::get_env_var_f64("PPS").expect("Could not get PPS environment variable");
    let flow_num = sender_receiver::get_env_var_f64("FLOW").expect("Could not get FLOW environment variable");

    let output = args.pop().unwrap_or_default();
    let input = args.pop().unwrap_or_default();

    if let Err(e) = sender_receiver::run(input, output, pps, flow_num as u8) {
        eprintln!("Application error: {e}");
        std::process::exit(1);
    }
}
