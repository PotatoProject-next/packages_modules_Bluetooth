extern crate clap;
#[macro_use]
extern crate lazy_static;

use clap::{App, AppSettings, Arg};
use dbus::{channel::MatchingReceiver, message::MatchRule};
use dbus_crossroads::Crossroads;
use dbus_tokio::connection;
use futures::future;
use log::LevelFilter;
use nix::sys::signal;
use std::error::Error;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use syslog::{BasicLogger, Facility, Formatter3164};
use tokio::time;

use bt_topshim::{btif::get_btinterface, topstack};
use btstack::{
    battery_manager::BatteryManager,
    battery_provider_manager::BatteryProviderManager,
    battery_service::BatteryService,
    bluetooth::{get_bt_dispatcher, Bluetooth, IBluetooth},
    bluetooth_gatt::BluetoothGatt,
    bluetooth_media::BluetoothMedia,
    socket_manager::BluetoothSocketManager,
    suspend::Suspend,
    Message, Stack,
};
use dbus_projection::DisconnectWatcher;
use tokio::sync::mpsc::Sender;

mod dbus_arg;
mod iface_battery_manager;
mod iface_battery_provider_manager;
mod iface_bluetooth;
mod iface_bluetooth_gatt;
mod iface_bluetooth_media;

const DBUS_SERVICE_NAME: &str = "org.chromium.bluetooth";

fn make_object_name(idx: i32, name: &str) -> String {
    String::from(format!("/org/chromium/bluetooth/hci{}/{}", idx, name))
}

/// Runs the Bluetooth daemon serving D-Bus IPC.
fn main() -> Result<(), Box<dyn Error>> {
    let matches = App::new("Bluetooth Adapter Daemon")
        // Allows multiple INIT_ flags to be given at the end of the arguments.
        .setting(AppSettings::TrailingVarArg)
        .arg(
            Arg::with_name("hci")
                .long("hci")
                .value_name("HCI")
                .takes_value(true)
                .help("The HCI index"),
        )
        .arg(Arg::with_name("debug").long("debug").short("d").help("Enables debug level logs"))
        .arg(Arg::from_usage("[init-flags] 'Fluoride INIT_ flags'").multiple(true))
        .arg(
            Arg::with_name("log-output")
                .long("log-output")
                .takes_value(true)
                .possible_values(&["syslog", "stderr"])
                .default_value("syslog")
                .help("Select log output"),
        )
        .get_matches();

    let is_debug = matches.is_present("debug");
    let log_output = matches.value_of("log-output").unwrap_or("syslog");

    let adapter_index = match matches.value_of("hci") {
        Some(idx) => idx.parse::<i32>().unwrap_or(0),
        None => 0,
    };

    // The remaining flags are passed down to Fluoride as is.
    let mut init_flags: Vec<String> = match matches.values_of("init-flags") {
        Some(args) => args.map(|s| String::from(s)).collect(),
        None => vec![],
    };

    // Set GD debug flag if debug is enabled.
    if is_debug {
        init_flags.push(String::from("INIT_logging_debug_enabled_for_all=true"));
    }

    // Forward --hci to Fluoride.
    init_flags.push(format!("--hci={}", adapter_index));

    let level_filter = if is_debug { LevelFilter::Debug } else { LevelFilter::Info };

    if log_output == "stderr" {
        env_logger::Builder::new().filter(None, level_filter).init();
    } else {
        let formatter = Formatter3164 {
            facility: Facility::LOG_USER,
            hostname: None,
            process: "btadapterd".into(),
            pid: 0,
        };

        let logger = syslog::unix(formatter).expect("could not connect to syslog");
        let _ = log::set_boxed_logger(Box::new(BasicLogger::new(logger)))
            .map(|()| log::set_max_level(level_filter));
    }

    let (tx, rx) = Stack::create_channel();
    let sig_notifier = Arc::new((Mutex::new(false), Condvar::new()));

    let intf = Arc::new(Mutex::new(get_btinterface().unwrap()));
    let bluetooth_gatt =
        Arc::new(Mutex::new(Box::new(BluetoothGatt::new(intf.clone(), tx.clone()))));
    let bluetooth_media =
        Arc::new(Mutex::new(Box::new(BluetoothMedia::new(tx.clone(), intf.clone()))));
    let battery_provider_manager =
        Arc::new(Mutex::new(Box::new(BatteryProviderManager::new(tx.clone()))));
    let battery_service = Arc::new(Mutex::new(Box::new(BatteryService::new(
        bluetooth_gatt.clone(),
        battery_provider_manager.clone(),
        tx.clone(),
    ))));
    let battery_manager = Arc::new(Mutex::new(Box::new(BatteryManager::new(
        battery_provider_manager.clone(),
        tx.clone(),
    ))));
    let bluetooth = Arc::new(Mutex::new(Box::new(Bluetooth::new(
        tx.clone(),
        intf.clone(),
        bluetooth_media.clone(),
        sig_notifier.clone(),
    ))));
    let suspend = Arc::new(Mutex::new(Box::new(Suspend::new(
        bluetooth.clone(),
        intf.clone(),
        bluetooth_gatt.clone(),
        tx.clone(),
    ))));

    let bt_sock_mgr = Arc::new(Mutex::new(Box::new(BluetoothSocketManager::new(tx.clone()))));

    topstack::get_runtime().block_on(async {
        // Connect to D-Bus system bus.
        let (resource, conn) = connection::new_system_sync()?;

        // The `resource` is a task that should be spawned onto a tokio compatible
        // reactor ASAP. If the resource ever finishes, we lost connection to D-Bus.
        tokio::spawn(async {
            let err = resource.await;
            panic!("Lost connection to D-Bus: {}", err);
        });

        // Request a service name and quit if not able to.
        conn.request_name(DBUS_SERVICE_NAME, false, true, false).await?;

        // Prepare D-Bus interfaces.
        let cr = Arc::new(Mutex::new(Crossroads::new()));
        cr.lock().unwrap().set_async_support(Some((
            conn.clone(),
            Box::new(|x| {
                tokio::spawn(x);
            }),
        )));

        // Announce the exported adapter objects so that clients can properly detect the readiness
        // of the adapter APIs.
        cr.lock().unwrap().set_object_manager_support(Some(conn.clone()));
        let object_manager = cr.lock().unwrap().object_manager();
        cr.lock().unwrap().insert("/", &[object_manager], {});

        // Run the stack main dispatch loop.
        topstack::get_runtime().spawn(Stack::dispatch(
            rx,
            bluetooth.clone(),
            bluetooth_gatt.clone(),
            battery_service.clone(),
            battery_manager.clone(),
            battery_provider_manager.clone(),
            bluetooth_media.clone(),
            suspend.clone(),
            bt_sock_mgr.clone(),
        ));

        // Set up the disconnect watcher to monitor client disconnects.
        let disconnect_watcher = Arc::new(Mutex::new(DisconnectWatcher::new()));
        disconnect_watcher.lock().unwrap().setup_watch(conn.clone()).await;

        // Set up handling of D-Bus methods. This must be done before exporting interfaces so that
        // clients that rely on InterfacesAdded signal can rely on us being ready to handle methods
        // on those exported interfaces.
        let cr_clone = cr.clone();
        conn.start_receive(
            MatchRule::new_method_call(),
            Box::new(move |msg, conn| {
                cr_clone.lock().unwrap().handle_message(msg, conn).unwrap();
                true
            }),
        );

        // Register D-Bus method handlers of IBluetooth.
        let adapter_iface = iface_bluetooth::export_bluetooth_dbus_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );
        let qa_iface = iface_bluetooth::export_bluetooth_qa_dbus_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );
        let socket_mgr_iface = iface_bluetooth::export_socket_mgr_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );
        let suspend_iface = iface_bluetooth::export_suspend_dbus_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );

        // Register D-Bus method handlers of IBluetoothGatt.
        let gatt_iface = iface_bluetooth_gatt::export_bluetooth_gatt_dbus_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );

        let media_iface = iface_bluetooth_media::export_bluetooth_media_dbus_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );

        let battery_provider_manager_iface =
            iface_battery_provider_manager::export_battery_provider_manager_dbus_intf(
                conn.clone(),
                &mut cr.lock().unwrap(),
                disconnect_watcher.clone(),
            );

        let battery_manager_iface = iface_battery_manager::export_battery_manager_dbus_intf(
            conn.clone(),
            &mut cr.lock().unwrap(),
            disconnect_watcher.clone(),
        );

        // Create mixin object for Bluetooth + Suspend interfaces.
        let mixin = Box::new(iface_bluetooth::BluetoothMixin {
            adapter: bluetooth.clone(),
            qa: bluetooth.clone(),
            suspend: suspend.clone(),
            socket_mgr: bt_sock_mgr.clone(),
        });

        cr.lock().unwrap().insert(
            make_object_name(adapter_index, "adapter"),
            &[adapter_iface, qa_iface, socket_mgr_iface, suspend_iface],
            mixin,
        );

        cr.lock().unwrap().insert(
            make_object_name(adapter_index, "gatt"),
            &[gatt_iface],
            bluetooth_gatt.clone(),
        );
        cr.lock().unwrap().insert(
            make_object_name(adapter_index, "media"),
            &[media_iface],
            bluetooth_media.clone(),
        );
        cr.lock().unwrap().insert(
            make_object_name(adapter_index, "battery_provider_manager"),
            &[battery_provider_manager_iface],
            battery_provider_manager.clone(),
        );
        cr.lock().unwrap().insert(
            make_object_name(adapter_index, "battery_manager"),
            &[battery_manager_iface],
            battery_manager.clone(),
        );

        // Hold locks and initialize all interfaces. This must be done AFTER DBus is
        // initialized so DBus can properly enforce user policies.
        {
            intf.lock().unwrap().initialize(get_bt_dispatcher(tx.clone()), init_flags);

            let adapter = bluetooth.clone();
            bluetooth_media.lock().unwrap().set_adapter(adapter.clone());

            let mut bluetooth = bluetooth.lock().unwrap();
            bluetooth.init_profiles();
            bluetooth.enable();

            bluetooth_gatt.lock().unwrap().init_profiles(tx.clone(), adapter.clone());
            // TODO(b/247093293): Gatt topshim api is only usable some
            // time after init. Investigate why this delay is needed
            // and make it a blocking part of init before removing
            // this.
            tokio::spawn(async move {
                time::sleep(Duration::from_millis(500)).await;
                battery_service.lock().unwrap().init();
            });
            bt_sock_mgr.lock().unwrap().initialize(intf.clone());

            // Install SIGTERM handler so that we can properly shutdown
            *SIG_DATA.lock().unwrap() = Some((tx.clone(), sig_notifier.clone()));

            let sig_action = signal::SigAction::new(
                signal::SigHandler::Handler(handle_sigterm),
                signal::SaFlags::empty(),
                signal::SigSet::empty(),
            );

            unsafe {
                signal::sigaction(signal::SIGTERM, &sig_action).unwrap();
            }
        }

        // Serve clients forever.
        future::pending::<()>().await;
        unreachable!()
    })
}

lazy_static! {
    /// Data needed for signal handling.
    static ref SIG_DATA: Mutex<Option<(Sender<Message>, Arc<(Mutex<bool>, Condvar)>)>> = Mutex::new(None);
}

extern "C" fn handle_sigterm(_signum: i32) {
    let guard = SIG_DATA.lock().unwrap();
    if let Some((tx, notifier)) = guard.as_ref() {
        log::debug!("Handling SIGTERM by disabling the adapter!");
        let txl = tx.clone();
        tokio::spawn(async move {
            // Send the shutdown message here.
            let _ = txl.send(Message::Shutdown).await;
        });

        let guard = notifier.0.lock().unwrap();
        if *guard {
            log::debug!("Waiting for stack to turn off for 2s");
            let _ = notifier.1.wait_timeout(guard, std::time::Duration::from_millis(2000));
        }
    }

    log::debug!("Sigterm completed");
    std::process::exit(0);
}
