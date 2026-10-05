//! Serial terminal: a port (USB serial, a router console, a board...) opened
//! as a terminal. Two threads: one reads from the port and sends the output
//! to the view; the other writes what is typed.

use std::io::{ErrorKind, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

use super::backend::{Backend, Cmd, Out};

/// Common speeds (baud).
pub const BAUD_RATES: &[u32] = &[
    300, 1200, 2400, 4800, 9600, 19200, 38400, 57600, 115200, 230400, 460800, 921600, 1500000,
];

/// Port and settings. Always 8 data bits, no parity and 1 stop bit (8N1),
/// which is what almost every device uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SerialParams {
    pub path: String,
    pub baud: u32,
}

/// A port found on the system.
#[derive(Debug, Clone)]
pub struct DetectedPort {
    pub path: String,
    /// Description (manufacturer and product, if USB).
    pub description: String,
}

/// Serial ports of the system, USB ones first.
pub fn available_ports() -> Vec<DetectedPort> {
    let mut ports: Vec<DetectedPort> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| {
            let description = match &p.port_type {
                serialport::SerialPortType::UsbPort(u) => {
                    [u.manufacturer.clone(), u.product.clone()]
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>()
                        .join(" ")
                }
                serialport::SerialPortType::BluetoothPort => "Bluetooth".into(),
                serialport::SerialPortType::PciPort => "PCI".into(),
                serialport::SerialPortType::Unknown => String::new(),
            };
            DetectedPort {
                path: p.port_name,
                description,
            }
        })
        // On macOS every port appears twice: /dev/cu.* is the outgoing one.
        .filter(|p| !p.path.starts_with("/dev/tty.") || !cfg!(target_os = "macos"))
        .collect();
    ports.sort_by_key(|p| (p.description.is_empty(), p.path.clone()));
    ports
}

/// Opens the port and starts reading.
pub fn start(params: SerialParams) -> (Backend, mpsc::UnboundedReceiver<Out>) {
    let (tx, mut cmd_rx) = mpsc::unbounded_channel::<Cmd>();
    let (out, out_rx) = mpsc::unbounded_channel();
    let backend = Backend::new(tx);
    let _ = out.send(Out::Status(
        t!(
            "terminal.serial.opening",
            path = params.path,
            baud = params.baud
        )
        .to_string(),
    ));

    let port = serialport::new(&params.path, params.baud)
        .data_bits(serialport::DataBits::Eight)
        .parity(serialport::Parity::None)
        .stop_bits(serialport::StopBits::One)
        .flow_control(serialport::FlowControl::None)
        .timeout(Duration::from_millis(100))
        .open();
    let mut reader = match port {
        Ok(p) => p,
        Err(e) => {
            let _ = out.send(Out::Failed(open_error(&params.path, &e)));
            return (backend, out_rx);
        }
    };
    let mut writer = match reader.try_clone() {
        Ok(w) => w,
        Err(e) => {
            let _ = out.send(Out::Failed(
                t!("terminal.serial.use_failed", path = params.path, error = e).to_string(),
            ));
            return (backend, out_rx);
        }
    };
    let _ = out.send(Out::Shell);
    let stop = Arc::new(AtomicBool::new(false));

    // Reading.
    {
        let tx_out = out.clone();
        let stop = stop.clone();
        let path = params.path.clone();
        let spawned = std::thread::Builder::new()
            .name("termoak-serial-read".into())
            .spawn(move || {
                let out = tx_out;
                let mut buf = [0u8; 4096];
                while !stop.load(Ordering::Acquire) {
                    match reader.read(&mut buf) {
                        Ok(0) => {}
                        Ok(n) => {
                            if out
                                .send(Out::Data(Bytes::copy_from_slice(&buf[..n])))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::TimedOut
                                    | ErrorKind::Interrupted
                                    | ErrorKind::WouldBlock
                            ) => {}
                        Err(e) => {
                            if !stop.load(Ordering::Acquire) {
                                let _ = out.send(Out::Closed(Some(
                                    t!("terminal.serial.lost", path = path, error = e).to_string(),
                                )));
                            }
                            break;
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            let _ = out.send(Out::Failed(
                t!("terminal.serial.reader_failed", error = e).to_string(),
            ));
        }
    }

    // Writing.
    let spawned = std::thread::Builder::new()
        .name("termoak-serial-write".into())
        .spawn(move || {
            while let Some(cmd) = cmd_rx.blocking_recv() {
                match cmd {
                    Cmd::Input(data) => {
                        if let Err(e) = writer.write_all(&data).and_then(|_| writer.flush()) {
                            let _ = out.send(Out::Closed(Some(
                                t!("terminal.serial.write_failed", error = e).to_string(),
                            )));
                            break;
                        }
                    }
                    // A serial port has no window size; sharing goes
                    // through the relay.
                    Cmd::Resize(..) | Cmd::Share(_) => {}
                    Cmd::Close | Cmd::CloseSession => break,
                }
            }
            stop.store(true, Ordering::Release);
        });
    if let Err(e) = spawned {
        eprintln!("could not create the serial port writer thread: {e}");
    }
    (backend, out_rx)
}

/// Clear message for the usual errors when opening a port.
fn open_error(path: &str, e: &serialport::Error) -> String {
    let text = e.to_string();
    let hint = match e.kind() {
        serialport::ErrorKind::NoDevice => Some(t!("terminal.serial.hint_no_device")),
        serialport::ErrorKind::Io(ErrorKind::PermissionDenied) => {
            if cfg!(target_os = "linux") {
                Some(t!("terminal.serial.hint_permission_linux"))
            } else {
                Some(t!("terminal.serial.hint_permission"))
            }
        }
        serialport::ErrorKind::Io(ErrorKind::NotFound) => {
            Some(t!("terminal.serial.hint_not_found"))
        }
        _ if text.to_lowercase().contains("busy") || text.contains("Access is denied") => {
            Some(t!("terminal.serial.hint_busy"))
        }
        _ => None,
    };
    let error = match hint {
        Some(hint) => format!("{text} ({hint})"),
        None => text,
    };
    t!("terminal.serial.open_failed", path = path, error = error).to_string()
}
