// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use rusb::{
    Device, DeviceHandle, Direction,  UsbContext,
};
use tauri::State;
use std::time::Duration;
use std::collections::HashSet;
use std::collections::HashMap;
use std::sync::Mutex;
use serde::{Serialize, Deserialize};
use std::io::Cursor;
use byteorder::{LittleEndian, ReadBytesExt};
use std::io::SeekFrom;
use std::io::Seek;
use std::default::Default;
use std::str;
use std::io::BufRead;
// Window shadow support
use tauri::Manager;
use window_shadows::set_shadow;

pub const LIBUSB_RECIPIENT_DEVICE: u8 = 0x00;
pub const LIBUSB_REQUEST_TYPE_VENDOR: u8 = 0x02 << 5;
pub const USB_TIMEOUT : Duration = Duration::from_millis(250);
const MAX_CFG_LEN: usize = 512;

#[derive(Debug)]
pub struct ConnectionState {
    serial_numbers: HashMap<u16, String>, // Maps addresses to serial numbers
    connected: Option<ConnectedDevice>
}

impl ConnectionState {
    fn new() -> ConnectionState {
        ConnectionState {
            serial_numbers: HashMap::new(),
            connected: None
        }
    }
}

#[derive(Debug)]
pub struct ConnectedDevice {
    device_handle: DeviceHandle<rusb::Context>,
    configuration_interface: Option<ConfigurationInterface>
}


#[derive(Debug)]
struct ConfigurationInterface {
    interface: u8,
    input: u8,
    output: u8
}

fn find_configuration_endpoints<T: UsbContext>(
    device: &Device<T>
) -> Option<ConfigurationInterface> {
    let device_desc = match device.device_descriptor() {
        Ok(d) => d,
        Err(_) => return None
    };
    for n in 0..device_desc.num_configurations() {
        let config_desc = match device.config_descriptor(n) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for interface in config_desc.interfaces() {
            for interface_desc in interface.descriptors() {
                if interface_desc.class_code() == 0xff
                {
                    let mut endpoints = ConfigurationInterface {interface: interface_desc.interface_number(), input: 0, output: 0};
                    let mut has_input = false;
                    let mut has_output = false;
                    for endpoint_desc in interface_desc.endpoint_descriptors() {
                        if endpoint_desc.direction() == Direction::In {
                            endpoints.input = endpoint_desc.address();
                            has_input = true;
                        }
                        if endpoint_desc.direction() == Direction::Out {
                            endpoints.output = endpoint_desc.address();
                            has_output = true;
                        }
                    }
                    if has_input && has_output {
                        return Some(endpoints)
                    }
                }
            }
        }
    }
    None
}

#[repr(u16)]
#[allow(dead_code)]
enum StructureTypes {
    // Commands/Responses, these are container TLVs. The Value will be a set of TLV structures.
    OK = 0,                     // Standard response when a command was successful
    NOK,                        // Standard error response
    FlashHeader,                // A special container for the config stored in flash. Hopefully there is some useful
                                // metadata in here to allow us to migrate an old config to a new version.
    GetVersion,                 // Returns the current config version, and the minimum supported version so clients
                                // can decide if they can talk to us or not.
    SetConfiguration,           // Updates the active configuration with the supplied TLVs
    GetActiveConfiguration,     // Retrieves the current active configuration TLVs from RAM
    GetStoredConfiguration,     // Retrieves the current stored configuration TLVs from Flash
    SaveConfiguration,          // Writes the active configuration to Flash
    FactoryReset,               // Invalidates the flash memory

    // Configuration structures, these are returned in the body of a command/response
    PreProcessingConfiguration = 0x200,
    FilterConfiguration,
    Pcm3060Configuration,

    // Status structures, these are returned in the body of a command/response but they are
    // not persisted as part of the configuration
    VersionStatus = 0x400,
}

enum FilterType {
    Lowpass = 0,
    Highpass,
    BandpassSkirt,
    BandpassPeak,
    Notch,
    Allpass,
    Peaking,
    LowShelf,
    HighShelf,
    CustomIIR
}

#[derive(Serialize, Deserialize, Default)]
struct Filter {
    filter_type: String,
    q: f32,
    f0: f32,
    db_gain: f32,
    a0: f64,
    a1: f64,
    a2: f64,
    b0: f64,
    b1: f64,
    b2: f64,
    enabled: bool
}

#[derive(Serialize, Deserialize, Default)]
struct Preprocessing {
    preamp: f32,
    reverse_stereo: bool
}

#[derive(Serialize, Deserialize, Default)]
struct Codec {
    oversampling: bool,
    phase: bool,
    rolloff: bool,
    de_emphasis: bool
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    preprocessing: Preprocessing,
    filters: Vec<Filter>,
    codec: Codec
}

#[derive(Serialize, Deserialize, Default)]
struct VersionInfo {
    current_version : u16,
    minimum_supported_version : u16,
    git_hash: String,
    pico_sdk_version: String
}

fn send_cmd(connection_state: State<'_, Mutex<ConnectionState>>, buf: &[u8]) -> Result<[u8; MAX_CFG_LEN], &'static str> {
    let connection = connection_state.lock().unwrap();
    match &connection.connected {
        Some(device) => {
            match &device.configuration_interface {
                Some(interface) => {
                    //println!("Write {} bytes to {}", buf.len(), interface.output);
                    match device.device_handle.write_bulk(interface.output, &buf, USB_TIMEOUT) {
                        Ok(_len) => (),
                        Err(err) => {
                            println!("Error {}", err);
                            return Err("Failed to write to the configuration interface");
                        }
                    }

                    let mut result = [0; MAX_CFG_LEN];
                    let mut read_length : u16 = 0;
                    let mut length : u16 = 4;
                    while read_length < length {
                        match device.device_handle.read_bulk(interface.input, &mut result, USB_TIMEOUT) {
                            Ok(len) => { 
                                //println!("Read {} {}/{}", len, read_length, length);
                                if read_length < 4 && len >=4 {
                                    let length_bytes : [u8; 2] = result[2..4].try_into().unwrap();
                                    length = u16::from_le_bytes(length_bytes);
                                    //println!("Length: {}", length);
                                    if usize::from(length) > MAX_CFG_LEN {
                                        println!("Overflow! {}", length);
                                        return Err("Overflow error");
                                    }
                                }
                                read_length += len as u16;
                            },
                            Err(err) => { 
                                println!("Read error {}", err);
                                return Err("Read Error");
                            }
                        }
                    }
                    return Ok(result);
                },
                None => {
                    println!("No configuration interface, update your headphones firmware");
                    return Err("No configuration interface");
                }
            }
        },
        None => {
            println!("The device is not connected.");
            return Err("Not connected");
        }
    }
}

#[tauri::command]
fn write_config(config: &str, connection_state: State<'_, Mutex<ConnectionState>>) -> Result<bool, ()> {
    let mut filter_payload : Vec<u8> = Vec::new();
    let mut preprocessing_payload : Vec<u8> = Vec::new();
    let mut codec_payload : Vec<u8> = Vec::new();
    match serde_json::from_str::<Config>(config) {
        Ok(cfg) => {
            for filter in cfg.filters.iter() {
                if filter.enabled {
                    let filter_type_val : u8;
                    let filter_args;

                    match filter.filter_type.as_str() {
                        "lowpass" => { filter_type_val = 0; filter_args = 2; },
                        "highpass" => { filter_type_val = 1; filter_args = 2; },
                        "bandpass_skirt" => { filter_type_val = 2; filter_args = 2; },
                        "bandpass" | "bandpass_peak" => { filter_type_val = 3; filter_args = 2; },
                        "notch" => { filter_type_val = 4; filter_args = 2; },
                        "allpass" => { filter_type_val = 5; filter_args = 2; },
                        "peaking" => { filter_type_val = 6; filter_args = 3; },
                        "lowshelf" => { filter_type_val = 7; filter_args = 3; },
                        "highshelf" => { filter_type_val = 8; filter_args = 3; },
                        "custom_iir" => { filter_type_val = 9; filter_args = 6; },
                        _ => return Ok(false)
                    }
                    filter_payload.push(filter_type_val);
                    filter_payload.extend_from_slice(&[0u8; 3]);
                    if filter_type_val == FilterType::CustomIIR as u8
                    {
                        filter_payload.extend_from_slice(&filter.a0.to_le_bytes());
                        filter_payload.extend_from_slice(&filter.a1.to_le_bytes());
                        filter_payload.extend_from_slice(&filter.a2.to_le_bytes());
                        filter_payload.extend_from_slice(&filter.b0.to_le_bytes());
                        filter_payload.extend_from_slice(&filter.b1.to_le_bytes());
                        filter_payload.extend_from_slice(&filter.b2.to_le_bytes());
                    }
                    else
                    {
                        filter_payload.extend_from_slice(&filter.f0.to_le_bytes());
                        if filter_args == 3 {
                            filter_payload.extend_from_slice(&filter.db_gain.to_le_bytes());
                        }
                        filter_payload.extend_from_slice(&filter.q.to_le_bytes());
                    }
                }
            }
            preprocessing_payload.extend_from_slice(&(cfg.preprocessing.preamp/100.0).to_le_bytes());
            preprocessing_payload.push(cfg.preprocessing.reverse_stereo as u8);
            preprocessing_payload.extend_from_slice(&[0u8; 3]);

            codec_payload.push(cfg.codec.oversampling as u8);
            codec_payload.push(cfg.codec.phase as u8);
            codec_payload.push(cfg.codec.rolloff as u8);
            codec_payload.push(cfg.codec.de_emphasis as u8);
        },
        Err(e) => {
            println!("Error: {}", e);
            return Ok(false);
        }
    }

    let mut buf : Vec<u8> = Vec::new();
    buf.extend_from_slice(&(StructureTypes::SetConfiguration as u16).to_le_bytes());
    buf.extend_from_slice(&((16+filter_payload.len()+preprocessing_payload.len()+codec_payload.len()) as u16).to_le_bytes());
    buf.extend_from_slice(&(StructureTypes::PreProcessingConfiguration as u16).to_le_bytes());
    buf.extend_from_slice(&((4+preprocessing_payload.len()) as u16).to_le_bytes());
    buf.extend_from_slice(&preprocessing_payload);
    buf.extend_from_slice(&(StructureTypes::FilterConfiguration as u16).to_le_bytes());
    buf.extend_from_slice(&((4+filter_payload.len()) as u16).to_le_bytes());
    buf.extend_from_slice(&filter_payload);
    buf.extend_from_slice(&(StructureTypes::Pcm3060Configuration as u16).to_le_bytes());
    buf.extend_from_slice(&((4+codec_payload.len()) as u16).to_le_bytes());
    buf.extend_from_slice(&codec_payload);
    
    match &send_cmd(connection_state, &buf) {
        Ok(_) => return Ok(true), // TODO: Check for NOK
        Err(_) => return Err(())
    }
}

#[tauri::command]
fn save_config(connection_state: State<'_, Mutex<ConnectionState>>) -> Result<bool, ()> {
    let mut buf : Vec<u8> = Vec::new();
    buf.extend_from_slice(&(StructureTypes::SaveConfiguration as u16).to_le_bytes());
    buf.extend_from_slice(&(4u16).to_le_bytes());

    match &send_cmd(connection_state, &buf) {
        Ok(_) => return Ok(true), // TODO: Check for NOK
        Err(_) => return Err(())
    }
}

#[tauri::command]
fn load_config(connection_state: State<'_, Mutex<ConnectionState>>) -> Result<String, ()> {
    let mut buf : Vec<u8> = Vec::new();
    buf.extend_from_slice(&(StructureTypes::GetStoredConfiguration as u16).to_le_bytes());
    buf.extend_from_slice(&(4u16).to_le_bytes());

    match &send_cmd(connection_state, &buf) {
        Ok(cfg) => {
            let mut cur = Cursor::new(cfg);
            let _result_type_val = cur.read_u16::<LittleEndian>().unwrap();
            let result_length_val = cur.read_u16::<LittleEndian>().unwrap();
            let mut position = 4;
            let mut cfg : Config = Default::default();
            while position < result_length_val {
                let type_val = cur.read_u16::<LittleEndian>().unwrap();
                let length_val = cur.read_u16::<LittleEndian>().unwrap();
                match type_val{
                    x if x == StructureTypes::PreProcessingConfiguration as u16 => {
                        cfg.preprocessing.preamp = cur.read_f32::<LittleEndian>().unwrap() * 100.0;
                        cfg.preprocessing.reverse_stereo = cur.read_u8().unwrap() != 0;
                        cur.seek(SeekFrom::Current(3)); // reserved bytes
                    },
                    x if x == StructureTypes::FilterConfiguration as u16 => {
                        let end = cur.position() + (length_val-4) as u64;
                        while cur.position() < end {
                            let filter_type = cur.read_u8().unwrap();
                            cur.seek(SeekFrom::Current(3)); // reserved bytes
                            let filter_args;

                            let mut filter : Filter = Default::default();
                            filter.enabled = true;
                            match filter_type {
                                x if x == FilterType::Lowpass as u8 => { filter.filter_type = "lowpass".to_string(); filter_args = 2; },
                                x if x == FilterType::Highpass as u8 => { filter.filter_type = "highpass".to_string(); filter_args = 2; },
                                x if x == FilterType::BandpassSkirt as u8 => { filter.filter_type = "bandpass_skirt".to_string(); filter_args = 2; },
                                x if x == FilterType::BandpassPeak as u8 => { filter.filter_type = "bandpass_peak".to_string(); filter_args = 2; },
                                x if x == FilterType::Notch as u8 => { filter.filter_type = "notch".to_string(); filter_args = 2; },
                                x if x == FilterType::Allpass as u8 => { filter.filter_type = "allpass".to_string(); filter_args = 2; },
                                x if x == FilterType::Peaking as u8 => { filter.filter_type = "peaking".to_string(); filter_args = 3; },
                                x if x == FilterType::LowShelf as u8 => { filter.filter_type = "lowshelf".to_string(); filter_args = 3; },
                                x if x == FilterType::HighShelf as u8 => { filter.filter_type = "highshelf".to_string(); filter_args = 3; },
                                x if x == FilterType::CustomIIR as u8 => { filter.filter_type = "custom_iir".to_string(); filter_args = 6; },
                                _ => return { println!("Unknown filter type {}", filter_type); Err(()) }
                            }

                            if filter_type == FilterType::CustomIIR as u8 {
                                filter.a0 = cur.read_f64::<LittleEndian>().unwrap();
                                filter.a1 = cur.read_f64::<LittleEndian>().unwrap();
                                filter.a2 = cur.read_f64::<LittleEndian>().unwrap();
                                filter.b0 = cur.read_f64::<LittleEndian>().unwrap();
                                filter.b1 = cur.read_f64::<LittleEndian>().unwrap();
                                filter.b2 = cur.read_f64::<LittleEndian>().unwrap();
                            }
                            else {
                                filter.f0 = cur.read_f32::<LittleEndian>().unwrap();
                                filter.db_gain;
                                if filter_args == 3 {
                                    filter.db_gain = cur.read_f32::<LittleEndian>().unwrap();
                                }
                                filter.q = cur.read_f32::<LittleEndian>().unwrap();
                            }
                            cfg.filters.push(filter)
                        }

                        if cur.position() != end {
                            println!("Read off the end of the filters TLV");
                            return Err(())
                        }
                    },
                    x if x == StructureTypes::Pcm3060Configuration as u16 => {
                        cfg.codec.oversampling = cur.read_u8().unwrap() != 0;
                        cfg.codec.phase = cur.read_u8().unwrap() != 0;
                        cfg.codec.rolloff = cur.read_u8().unwrap() != 0;
                        cfg.codec.de_emphasis = cur.read_u8().unwrap() != 0;
                    },
                    _ => {
                        println!("Unsupported TLV type {}", type_val);
                    }
                }
                //println!("\tT: {} L: {}", type_val, length_val);
                position += length_val;
                cur.set_position(position as u64);
            }
            return Ok(serde_json::to_string(&cfg).unwrap())
        }, // TODO: Check for NOK
        Err(_) => return Err(())
    }
}

#[tauri::command]
fn factory_reset(connection_state: State<'_, Mutex<ConnectionState>>) -> Result<bool, ()> {
    let mut buf : Vec<u8> = Vec::new();
    buf.extend_from_slice(&(StructureTypes::FactoryReset as u16).to_le_bytes());
    buf.extend_from_slice(&(4u16).to_le_bytes());

    match &send_cmd(connection_state, &buf) {
        Ok(_r) => { return Ok(true) }, // TODO: Check for NOK
        Err(_) => { println!("Factory Reset Err"); return Err(()) }
    }
}

#[tauri::command]
fn reboot_bootloader(connection_state: State<Mutex<ConnectionState>>) -> bool {
    let connection = connection_state.lock().unwrap();
    match &connection.connected {
        Some(device) => {
            let buf : [u8;0] = [];
            let r = device.device_handle.write_control(LIBUSB_RECIPIENT_DEVICE | LIBUSB_REQUEST_TYPE_VENDOR, 0, 0x2e8a, 0, &buf, USB_TIMEOUT);
            println!("Reboot Device: {}", r.is_err());

            return true;
        },
        None => {
            println!("No connection");
            return false;
        }
    }
}

#[tauri::command]
fn read_version_info(connection_state: State<'_, Mutex<ConnectionState>>) -> Result<String, ()> {
    let mut buf : Vec<u8> = Vec::new();
    buf.extend_from_slice(&(StructureTypes::GetVersion as u16).to_le_bytes());
    buf.extend_from_slice(&(4u16).to_le_bytes());

    match &send_cmd(connection_state, &buf) {
        Ok(v) => {
            let mut cur = Cursor::new(v);
            let _result_type_val = cur.read_u16::<LittleEndian>().unwrap();
            let _result_length_val = cur.read_u16::<LittleEndian>().unwrap();

            let _version_tlv_type_val = cur.read_u16::<LittleEndian>().unwrap();
            let _version_tlv_length_val = cur.read_u16::<LittleEndian>().unwrap();

            let mut versions : VersionInfo = Default::default();
            versions.current_version = cur.read_u16::<LittleEndian>().unwrap();
            versions.minimum_supported_version = cur.read_u16::<LittleEndian>().unwrap();
            cur.consume(4);
            let mut str_buf : Vec<u8> = Vec::new();
            cur.read_until(0u8, &mut str_buf).unwrap();
            str_buf.pop();
            match str::from_utf8(&str_buf) {
                Ok(s) => versions.git_hash = s.to_string(),
                Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
            };
            str_buf.clear();
            cur.read_until(0u8, &mut str_buf).unwrap();
            str_buf.pop();
            match str::from_utf8(&str_buf) {
                Ok(s) => versions.pico_sdk_version = s.to_string(),
                Err(e) => panic!("Invalid UTF-8 sequence: {}", e),
            };
            return Ok(serde_json::to_string(&versions).unwrap())
        }, // TODO: Check for NOK
        Err(_) => return Err(())
    }
}
#[tauri::command]
fn open(serial_number: &str, connection_state: State<Mutex<ConnectionState>>) -> bool {
    let context = match rusb::Context::new() {
        Ok(c) => c,
        Err(e) => panic!("libusb::Context::new(): {}", e),
    };

    let devices = match context.devices() {
        Ok(d) => d,
        Err(_) => return false,
    };

    let mut connection = connection_state.lock().unwrap();
    connection.connected = None;
    for device in devices.iter() {
        let address : u16 = ((device.bus_number() as u16) << 8) | (device.address() as u16);
        match connection.serial_numbers.get(&address) {
            Some(sn) => {
                if sn == serial_number {
                    match device.open() {
                        Ok(mut handle) => {
                            let configuration_interface = find_configuration_endpoints(&device);
                            match &configuration_interface {
                                Some(i) => { handle.claim_interface(i.interface).unwrap(); },
                                None => { println!("Could not detect a configuration interface"); return false; }
                            }
                            connection.connected = Some(ConnectedDevice {device_handle: handle, configuration_interface: configuration_interface });
                            return true
                        },
                        Err(e) => {
                            println!("Could not open {}", e);
                            return false
                        }
                    }
                }
            },
            None => continue
        }
    }
    return false;
}

#[tauri::command]
fn poll_devices(connection_state: State<Mutex<ConnectionState>>) -> String {
    let mut device_list = Vec::with_capacity(10);
    let mut known_devices : HashSet<u16> = connection_state.lock().unwrap().serial_numbers.keys().cloned().collect();

    let context = match rusb::Context::new() {
        Ok(c) => c,
        Err(e) => panic!("libusb::Context::new(): {}", e),
    };

    let devices = match context.devices() {
        Ok(d) => d,
        Err(_) => return serde_json::to_string(&device_list).unwrap(),
    };

    for device in devices.iter() {
        let address : u16 = ((device.bus_number() as u16) << 8) | (device.address() as u16);
        if known_devices.contains(&address) {
            device_list.push(connection_state.lock().unwrap().serial_numbers[&address].clone());
            known_devices.remove(&address);
            continue
        }
        let device_desc = match device.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        // println!("Device {:#x}:{:#x} {:#x} {:#x} {:#x}", device_desc.vendor_id(), device_desc.product_id(), device_desc.class_code(), device.bus_number(), device.address());

        if device_desc.vendor_id() == 0x2e8a && device_desc.product_id() == 0xfedd {
            match device.open() {
                Ok(handle) => {
                    let serial_number_string_index = device_desc.serial_number_string_index().unwrap();
                    let serial_number = handle.read_string_descriptor_ascii(serial_number_string_index);
                    match serial_number {
                        Ok(sn) => {
                            connection_state.lock().unwrap().serial_numbers.insert(address, sn.clone());
                            device_list.push(sn);
                        },
                        Err(e) => {
                            println!("Get serial number failed {}", e);
                            continue
                        }
                    }
                },
                Err(e) => {
                    println!("Open failed {}", e);
                    continue
                }
            }
        }
    }

    // Handle unplugged devices
    for address in known_devices
    {
        connection_state.lock().unwrap().serial_numbers.remove(&address);
    }

    serde_json::to_string(&device_list).unwrap()
}

fn main() {
    tauri::Builder::default().setup(|app| {
            let window = app.get_window("main").unwrap();
            #[cfg(any(windows, target_os = "macos"))]
            set_shadow(&window, true).expect("Unsupported platform!");
            Ok(())
        })
        .manage(Mutex::new(ConnectionState::new()))
        .invoke_handler(tauri::generate_handler![reboot_bootloader, poll_devices, open, write_config, save_config, factory_reset, load_config, read_version_info])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
