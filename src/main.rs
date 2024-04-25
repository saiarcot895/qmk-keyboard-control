use tokio_stream::StreamExt;
use tokio::io::{Interest, unix::AsyncFd, Ready};
use std::error::Error;
use hidapi::{HidApi, HidDevice};
use zbus::{Connection, interface, proxy};
use std::sync::{Arc, Mutex};
use std::collections::{HashMap, HashSet};
use udev::{Event, MonitorBuilder};

#[derive(Default)]
struct LinuxQmkConnector {
    devices: HashMap<(u16, u16), HidDevice>,
}

const GET_COMMAND : u8 = 0x08;
const SET_COMMAND : u8 = 0x07;

const ENABLE_BACKLIGHT : u8 = 0x00;
const ACTIVE_MODE_RGB  : u8 = 0x01;
const RGB_SPEED        : u8 = 0x02;
const GAME             : u8 = 0x03;

const SUPPORTED_DEVICES : [(u16, u16, u16, u16); 2] = [(0x3297u16, 0x1969u16, 0xFF60u16, 0x61u16),
                                                       (0x3434u16, 0x0120u16, 0xFF60u16, 0x61u16)];

impl LinuxQmkConnector {

    fn connect_to_known_devices(&mut self) -> Result<(), Box<dyn Error>> {
        let supported_devices_hash = HashSet::from(SUPPORTED_DEVICES);
        match HidApi::new() {
            Ok(api) => {
                for device in api.device_list() {
                    if supported_devices_hash.contains(&(device.vendor_id(), device.product_id(), device.usage_page(), device.usage())) {
                        let device_handle = device.open_device(&api)?;
                        self.devices.insert((device.vendor_id(), device.product_id()), device_handle);
                    }
                }
            },
            Err(e) => {
                return Err(Box::new(e));
            },
        }
        Ok(())
    }

    fn connect_to_device(&mut self, vid: u16, pid: u16, usage_page: u16, usage: u16) -> Result<(), Box<dyn Error>> {
        match HidApi::new() {
            Ok(mut api) => {
                api.reset_devices()?;
                api.add_devices(vid, pid)?;
                for device in api.device_list() {
                    if device.usage_page() == usage_page && device.usage() == usage {
                        let device_handle = device.open_device(&api)?;
                        self.devices.insert((vid, pid), device_handle);
                    }
                }
            },
            Err(e) => {
                return Err(Box::new(e));
            },
        }
        Ok(())
    }

    fn disconnect_from_device(&mut self, vid: u16, pid: u16) {
        self.devices.remove(&(vid, pid));
    }

    fn send_message(&mut self, buf: &mut [u8]) {
        let mut message_payload : [u8; 32] = [0; 32];
        message_payload[1..buf.len() + 1].copy_from_slice(buf);
        self.devices.retain(|_, device| {
            match device.write(&message_payload) {
                Ok(_) => {
                    return true;
                },
                Err(e) => {
                    eprintln!("Error in writing to device: {}", e);
                    return false;
                }
            }
        });
    }

    fn game_changed(&mut self, game_value: u8) {
        self.send_message(&mut [SET_COMMAND, GAME, game_value]);
    }

    fn screensaver_state_changed(&mut self, locked: &bool) {
        self.send_message(&mut [SET_COMMAND, ACTIVE_MODE_RGB, if *locked { 0x01 } else { 0x00 }]);
    }
}

struct ActiveWindowMonitor {
    connector: Arc<Mutex<LinuxQmkConnector>>,
    current_game: u8,
}

#[interface(name = "com.saikrishna.ActiveWindowMonitor")]
impl ActiveWindowMonitor {
    fn new_active_window(&mut self, window_name: &str) {
        let mut new_current_game : u8 = 0;
        if window_name == "Deep Rock Galactic" {
            new_current_game = 1;
        } else if window_name == "Don't Starve Together" {
            new_current_game = 2;
        }
        if self.current_game != new_current_game {
            self.current_game = new_current_game;
            self.connector.lock().unwrap().game_changed(self.current_game);
        }
    }
}

#[proxy(
    default_service = "org.freedesktop.ScreenSaver",
    default_path = "/ScreenSaver",
    interface = "org.freedesktop.ScreenSaver"
)]
trait ScreenSaver {
    #[zbus(signal)]
    fn active_changed(&self, is_active: bool) -> zbus::Result<()>;
}

async fn wait_for_screensaver_state_change(mut active_changed_stream: ActiveChangedStream<'_>, linux_qmk_connector: Arc<Mutex<LinuxQmkConnector>>) {
    while let Some(msg) = active_changed_stream.next().await {
        let args : ActiveChangedArgs = msg.args().expect("Error parsing message");
        linux_qmk_connector.lock().unwrap().screensaver_state_changed(&args.is_active);
    }
}

fn new_device_connected(linux_qmk_connector: &Arc<Mutex<LinuxQmkConnector>>, event: Event) {
    match event.event_type() {
        udev::EventType::Bind => {
            let product_str : &str = event.property_value("PRODUCT").expect("Missing PRODUCT property").to_str().expect("Invalid input");
            let product : Vec<&str> = product_str.split('/').collect();
            let vendor_id = u16::from_str_radix(product[0], 16).expect("Invalid vendor ID");
            let product_id = u16::from_str_radix(product[1], 16).expect("Invalid product ID");
            match linux_qmk_connector.lock().unwrap().connect_to_device(vendor_id, product_id, 0xFF60u16, 0x61u16) {
                Ok(()) => {},
                Err(e) => {
                    eprintln!("Error in connecting to devices: {}", e);
                }
            }
        }
        udev::EventType::Unbind => {
            let product_str : &str = event.property_value("PRODUCT").expect("Missing PRODUCT property").to_str().expect("Invalid input");
            let product : Vec<&str> = product_str.split('/').collect();
            let vendor_id = u16::from_str_radix(product[0], 16).expect("Invalid vendor ID");
            let product_id = u16::from_str_radix(product[1], 16).expect("Invalid product ID");
            linux_qmk_connector.lock().unwrap().disconnect_from_device(vendor_id, product_id);
        }
        _ => {}
    }
}

async fn listen_for_devices(linux_qmk_connector: Arc<Mutex<LinuxQmkConnector>>) -> Result<(), Box<dyn Error>> {
    let socket = MonitorBuilder::new().expect("Unable to open socket to listen for udev events")
        .match_subsystem_devtype("usb", "usb_device").expect("Unable to filter udev events")
        .listen().expect("Unable to listen for udev events");
    let mut socket = AsyncFd::new(socket).expect("Unable to get socket information");
    loop {
        let mut guard = socket 
            .ready_mut(Interest::READABLE)
            .await?;

        if guard.ready().is_readable() {
            match guard.get_inner_mut().iter().next() {
                Some(event) => {
                    new_device_connected(&linux_qmk_connector, event);
                }
                None => {
                    guard.clear_ready_matching(Ready::READABLE);
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let linux_qmk_connector : Arc<Mutex<LinuxQmkConnector>> = Default::default();
    match linux_qmk_connector.lock().unwrap().connect_to_known_devices() {
        Ok(()) => {
        },
        Err(e) => {
            eprintln!("Error in getting USB HID devices: {}", e);
            return Err(e);
        }
    }
    let connection = Connection::session().await?;
    // setup the server
    connection
        .object_server()
        .at("/com/saiarcot895/ActiveWindowMonitor", ActiveWindowMonitor { connector: linux_qmk_connector.clone(), current_game: 0 })
        .await?;
    // before requesting the name
    connection
        .request_name("com.saiarcot895.ActiveWindowMonitor")
        .await?;
    
    let screensaver_proxy = ScreenSaverProxy::new(&connection).await?;
    let active_changed_stream = screensaver_proxy.receive_active_changed().await?;

    tokio::join!(
        wait_for_screensaver_state_change(active_changed_stream, linux_qmk_connector.clone()),
        listen_for_devices(linux_qmk_connector),
        );

    Ok(())
}
