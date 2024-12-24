use tokio_stream::StreamExt;
use tokio::io::{Interest, unix::AsyncFd, Ready};
use hidapi::{HidApi, HidDevice, DeviceInfo};
use zbus::{Connection, interface, proxy};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use udev::{DeviceType, Event, MonitorBuilder};
use log::{error, info, debug};
use systemd_journal_logger::{connected_to_journal, JournalLog};
use simplelog::*;
use std::{ffi::CString, fs, os::linux::fs::MetadataExt};
use thiserror::Error;

#[derive(Debug)]
struct SimpleDeviceInfo {
    vendor_id: u16,
    product_id: u16,
    product_name: String,
    device_name: String,
    device_handle: HidDevice,
}

impl SimpleDeviceInfo {
    fn from_device_info(device: &DeviceInfo, device_handle: HidDevice) -> Self {
        Self {
            vendor_id: device.vendor_id(),
            product_id: device.product_id(),
            product_name: device.product_string().unwrap_or("Unknown").to_string(),
            device_name: String::from_utf8_lossy(device.path().to_bytes()).to_string(),
            device_handle: device_handle,
        }
    }
}

#[derive(Default, Debug)]
struct LinuxQmkConnector {
    devices: HashMap<String, SimpleDeviceInfo>,
}

#[derive(Error, Debug)]
enum ConnectorError {
    #[error("error from libhid")]
    HidError(#[from] hidapi::HidError),
    #[error("error from udev")]
    UdevError(#[from] std::io::Error),
    #[error("error from dbus")]
    DbusError(#[from] zbus::Error),
}

const GET_COMMAND : u8 = 0x08;
const SET_COMMAND : u8 = 0x07;

const ENABLE_BACKLIGHT : u8 = 0x80;
const ACTIVE_MODE_RGB  : u8 = 0x81;
const RGB_SPEED        : u8 = 0x82;
const GAME             : u8 = 0x83;

impl LinuxQmkConnector {

    fn connect_to_device_with_device_info(&mut self, device: &DeviceInfo, api: &HidApi) -> Result<(), ConnectorError> {
        let device_handle = device.open_device(&api)?;
        let device_info = SimpleDeviceInfo::from_device_info(device, device_handle);
        let metadata = fs::metadata(&device_info.device_name)?;
        match udev::Device::from_devnum(DeviceType::Character, metadata.st_rdev()) {
            Ok(udev_device_info) => {
                info!("Connected to {} ({:x}:{:x}) via {:#?}", device_info.product_name, device_info.vendor_id, device_info.product_id, udev_device_info.devpath());
                self.devices.insert(udev_device_info.devpath().to_string_lossy().to_string(), device_info);
            }
            Err(e) => {
                error!("Error getting device information from udev for {}: {}", device_info.device_name, e);
                return Err(ConnectorError::UdevError(e));
            }
        }
        Ok(())
    }

    fn connect_to_known_devices(&mut self) -> Result<(), ConnectorError> {
        let api = HidApi::new()?;
        for device in api.device_list() {
            if device.usage_page() == 0xFF60u16 && device.usage() == 0x61u16 {
                self.connect_to_device_with_device_info(&device, &api);
            }
        }
        Ok(())
    }

    fn connect_to_device_with_hid_path(&mut self, path: &str) -> Result<(), ConnectorError> {
        let mut api = HidApi::new()?;
        api.refresh_devices()?;
        for device in api.device_list() {
            if device.path() == CString::new(path).unwrap().as_c_str() && device.usage_page() == 0xFF60u16 && device.usage() == 0x61u16 {
                return self.connect_to_device_with_device_info(&device, &api);
            }
        }
        Ok(())
    }

    fn disconnect_from_device(&mut self, device_path: &str) {
        match self.devices.remove(device_path) {
            Some(device_info) => {
                info!("Disconnected from {} ({:x}:{:x})", device_info.product_name, device_info.vendor_id, device_info.product_id);
            },
            None => {
                debug!("No connection to {device_path} found");
            }
        }
    }

    fn send_message(&mut self, buf: &[u8]) {
        let mut message_payload : [u8; 33] = [0; 33];
        message_payload[1..buf.len() + 1].copy_from_slice(buf);
        self.devices.retain(|_, device| {
            match device.device_handle.write(&message_payload) {
                Ok(num_bytes) => {
                    debug!("Wrote {} bytes", num_bytes);
                    if (num_bytes < buf.len()) {
                        error!("Error in writing all bytes to device. Only {} bytes written", num_bytes);
                        return false;
                    }
                },
                Err(e) => {
                    error!("Error in writing to device: {}", e);
                    return false;
                }
            }
            let mut response_payload : [u8; 32] = [0; 32];
            match device.device_handle.read_timeout(&mut response_payload, 2000) {
                Ok(num_bytes) => {
                    debug!("Read {} bytes", num_bytes);
                    if (num_bytes < buf.len()) {
                        error!("Error in getting response from device. Only {} bytes received", num_bytes);
                        return false;
                    }
                    return true;
                },
                Err(e) => {
                    error!("Error in getting response from device: {}", e);
                    return false;
                }
            }
        });
    }

    fn game_changed(&mut self, game_value: u8) {
        self.send_message(&[SET_COMMAND, GAME, game_value]);
    }

    fn screensaver_state_changed(&mut self, locked: &bool) {
        self.send_message(&[SET_COMMAND, ACTIVE_MODE_RGB, if *locked { 0x01 } else { 0x00 }]);
    }
}

struct ActiveWindowMonitor {
    connector: Arc<Mutex<LinuxQmkConnector>>,
    current_game: u8,
}

#[interface(name = "com.saiarcot895.ActiveWindowMonitor")]
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
        udev::EventType::Add => {
            match linux_qmk_connector.lock().unwrap().connect_to_device_with_hid_path(&event.devnode().unwrap().to_string_lossy()) {
                Ok(()) => {},
                Err(e) => {
                    error!("Error in connecting to device: {}", e);
                }
            }
        }
        udev::EventType::Remove => {
            linux_qmk_connector.lock().unwrap().disconnect_from_device(&event.device().devpath().to_string_lossy());
        }
        _ => {}
    }
}

async fn listen_for_devices(linux_qmk_connector: Arc<Mutex<LinuxQmkConnector>>) -> Result<(), ConnectorError> {
    let socket = MonitorBuilder::new().expect("Unable to open socket to listen for udev events")
        .match_subsystem("hidraw").expect("Unable to filter udev events")
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
async fn main() -> Result<(), ConnectorError> {
    if connected_to_journal() {
        // If the output streams of this process are directly connected to the
        // systemd journal log directly to the journal to preserve structured
        // log entries (e.g. proper multiline messages, metadata fields, etc.)
        JournalLog::new()
            .unwrap()
            .install()
            .unwrap();
    } else {
        // Otherwise fall back to logging with stderrlog.
        TermLogger::init(LevelFilter::Info, Config::default(), TerminalMode::Mixed, ColorChoice::Auto)
            .unwrap();
    }

    log::set_max_level(LevelFilter::Info);

    let linux_qmk_connector : Arc<Mutex<LinuxQmkConnector>> = Default::default();
    match linux_qmk_connector.lock().unwrap().connect_to_known_devices() {
        Ok(()) => {
        },
        Err(e) => {
            error!("Error in getting USB HID devices: {}", e);
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
