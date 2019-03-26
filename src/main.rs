#[macro_use]
extern crate serde_derive;
extern crate docopt;
extern crate mosquitto_client as mosq;
extern crate regex;
extern crate tokio;
extern crate futures;

pub mod parse;
pub mod decode;
pub mod ppp_packet;

use docopt::Docopt;
use std::error::Error;
use std::io::prelude::*;
use std::fs::File;
use std::path::Path;
use std::collections::HashMap;
use std::mem::size_of;
use regex::bytes::Regex;
use mosq::Mosquitto;

use parse::parse;
use decode::decode;

const USAGE: &'static str = "
si2mqtt: Read data off the SMA Sunny Island RS485 display bus
         and send it to an MQTT broker.

Usage:
  si2mqtt -p <tty> -m <broker> [-t <topic>]
  si2mqtt -h | --help

Options:
  -p, --port=<port>     The serial port from which to read RS485 data
  -m, --mqtt=<broker>   The MQTT broker to which to connect
  -t, --topic=<topic>   The MQTT topic under which to publish our data
  -h, --help            Show this documentation
";

#[derive(Deserialize)]
struct Args {
	flag_port: String,
	flag_mqtt: String,
	flag_topic: String,
}

fn main() {
	let mut status: Arc<HashMap<&str, String>> = Arc::new(HashMap::new());
	/* 
		flow: charge|discharge,
		load: 0.0,
		genset/engaged: false,
		genset/requested: false,
		genset/output: 0.0,
		battery/fan: false,
		battery/charge: 100,
		battery/health: 100,
	*/
	let args: Arc<Args> = Arc::new(
		Docopt::new(USAGE)
		.and_then(|d| d.deserialize())
		.unwrap_or_else(|e| e.exit())
	);

	let m = Arc::new(Mosquitto::new("test"));
	m.connect(&args.flag_mqtt, 1883).expect("Cannot connect to mqtt broker");


	let timer = Interval::new(Instant::now(), Duration::from_millis(5000))
		.for_each(move |_t| {
			let m = Arc::clone(&m);
			let args = Arc::clone(&args);
			let status = Arc::clone(&status);
			mqtt_publish(&m, &args.flag_topic, &status);
			Ok(())
		})
		.map_err(|e| panic!("timer error {:?}", e));


	let path = Path::new(&args.flag_port);
	let filename = Arc::new(path);
	let port = tokio::fs::File::open(Arc::clone(&filename))
		.and_then(|file| { tokio::spawn(timer); Ok(file) })
		.and_then(move |file| read_data(file, &mut Arc::clone(&status)))
		.map_err(|e| panic!("Unable to open file.txt {:?}", e));

	tokio::run(port);
}

fn read_data(mut file: tokio::fs::File, mut status: &mut HashMap<&str, String>) -> Result<(), Error> {
	let mut buf = Vec::new();
	loop {
		let mut chunk = vec![0u8; 40];
		let bytes = match file.poll_read(&mut chunk) {
			Ok(Async::Ready(n)) => n,
			Ok(Async::NotReady) => 0,
			Err(e) => return Err(e)
		};
		if bytes == 0 {
			break;
		}
		chunk.truncate(bytes);
		buf.append(&mut chunk);
		for packet in parse(&mut buf) {
			decode(packet, &mut status);
		}
	}
	Ok(())
}

fn parse(chunk: &mut Vec<u8>) -> Vec<Packet> {
	println!("Decoding {} bytes", chunk.len());
	for b in chunk.iter() {
		print!("{:00x} ", b);
	}
	println!("");
	let mut packets: Vec<Packet> = Vec::new();
	let mut iter: usize = 0;
	let mut packet_start: usize = 0;
	while iter < chunk.len() - size_of::<Header>() {
		if chunk[iter] == 0x7e && chunk[iter+1] == 0xff {
			let header: *const u8 = chunk[iter..].as_ptr();
			let header: &Header = unsafe { &*(header as *const Header) };
			let payload = iter + size_of::<Header>();
			if header.mode == 0x0b {
				// This is a display update
				for null in payload .. chunk.len() - 3 { // 3 bytes allowance for checksum/terminator
					if chunk[null] == 0x00 {
						// End of payload
						packets.push(Packet {
							header: header,
							payload: chunk[payload .. null].to_vec(),
							_checksum: ((chunk[null+1] as u16) << 8) + (chunk[null+2] as u16),
						});
						iter = null + 3;
						packet_start = iter + 1;
						break;
					}
				}
			} else {
				// println!("{:?}", header);
			}
		}
		iter += 1;
	}
	println!("Returning {} unprocessed bytes", chunk.len() - packet_start);
	*chunk = chunk[packet_start..].to_vec();
	return packets;
}

fn decode(packet: Packet, status: &mut HashMap<&str, String>) -> () {
	match packet.header.row {
		1 => {
			status.insert("genset/engaged",
				match packet.payload[0] == 0x03 && packet.payload[4] != 0xa4 {
					true  => "1".to_string(),
					false => "0".to_string(),
				}
			);
		},
		2 => {
			let re = Regex::new(r"^(\d+\.\d)kW\s+(\x01|\x02)\s+(-?\d+\.\d+)kW.+?(o|\x06)(o|\x06)").unwrap();
			let caps: Vec<String> =
				re.captures(packet.payload.as_slice())
				  .unwrap()
				  .iter()
				  .map(|c|
					String::from_utf8(
						c.unwrap()
						 .as_bytes()
						 .to_vec())
					.unwrap())
				  .collect();
			status.insert("genset/output", caps[1].parse().unwrap());
			status.insert("flow",
				match caps[2].as_str() {
					"\u{1}" => "charge".to_string(),
					"\u{2}" => "discharge".to_string(),
					_       => "unknown".to_string(),
				}
			);
			status.insert("load", caps[3].parse().unwrap());
			status.insert("battery/fan", match caps[4] != "o" { true => "1".to_string(), false => "0".to_string() });
			status.insert("genset/requested", match caps[5] != "o" { true => "1".to_string(), false => "0".to_string() });
		},
		3 => {
			let re = Regex::new(r"^[\*!\?]").unwrap();
			let engaged = match re.captures(packet.payload.as_slice()) {
				Some(c) => {
					let s = status.clone();
					let output = match s.get(&"genset/output") {
						Some(o) => o,
						None    => "",
					};
					match c[0][0] {
						33 if output == "0.0" => "0",
						_ => "1"
					}
				},
				None => {
					"0"
				}
			};
			status.insert("genset/engaged", engaged.to_string());
		},
		4 => {
			let re = Regex::new(r"\s+(\d+)%\s+\d{2}:\d{2}:\d{2}").unwrap();
			let caps = re.captures(packet.payload.as_slice()).unwrap();
			status.insert("battery/charge", String::from_utf8(caps[1].to_vec()).unwrap().parse().unwrap());
		},
		_ => {},
	}
}

fn mqtt_publish(m: &Mosquitto, topic: &String, status: &HashMap<&str, String>) -> Result<(), std::io::Error> {
	println!("STATUS {:?}", status);
	// for (k,v) in status {
	// 	let t = format!("{}/{}", topic, k);
	// 	let _mid = m.publish(t.as_str(), v.as_bytes(), 2, false);
	// }
	Ok(())
}
