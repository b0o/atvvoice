//! D-Bus control interface for atvvoice.
//!
//! Exposes `org.atvvoice.Daemon` on the session bus, allowing external programs
//! to query status and control the microphone.
//!
//! Requires the `dbus` cargo feature (enabled by default).

use tokio::sync::{mpsc, watch};
use zbus::interface;

use crate::atvv::{ExternalCommand, State};

/// Static info about the daemon, set at startup.
#[derive(Clone)]
pub struct DaemonInfo {
    pub device_address: String,
    pub node_name: String,
}

/// D-Bus interface implementation.
pub struct DaemonInterface {
    command_tx: mpsc::Sender<ExternalCommand>,
    state_rx: watch::Receiver<State>,
    info: DaemonInfo,
}

impl DaemonInterface {
    pub fn new(
        command_tx: mpsc::Sender<ExternalCommand>,
        state_rx: watch::Receiver<State>,
        info: DaemonInfo,
    ) -> Self {
        Self {
            command_tx,
            state_rx,
            info,
        }
    }
}

#[interface(name = "org.atvvoice.Daemon")]
impl DaemonInterface {
    /// Open the microphone (start streaming).
    async fn mic_open(&self) -> zbus::fdo::Result<()> {
        self.command_tx
            .send(ExternalCommand::MicOpen)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("send failed: {e}")))?;
        Ok(())
    }

    /// Close the microphone (stop streaming).
    async fn mic_close(&self) -> zbus::fdo::Result<()> {
        self.command_tx
            .send(ExternalCommand::MicClose)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("send failed: {e}")))?;
        Ok(())
    }

    /// Toggle the microphone based on current state.
    async fn mic_toggle(&self) -> zbus::fdo::Result<()> {
        self.command_tx
            .send(ExternalCommand::MicToggle)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("send failed: {e}")))?;
        Ok(())
    }

    /// Current state: "init", "ready", "opening", "streaming".
    #[zbus(property)]
    async fn state(&self) -> String {
        state_to_str(*self.state_rx.borrow()).to_string()
    }

    /// Bluetooth address of the connected remote.
    #[zbus(property)]
    async fn device_address(&self) -> String {
        self.info.device_address.clone()
    }

    /// PipeWire node name.
    #[zbus(property)]
    async fn node_name(&self) -> String {
        self.info.node_name.clone()
    }

    /// Emitted when the mic state changes.
    #[zbus(signal)]
    pub async fn mic_state_changed(
        ctxt: &zbus::object_server::SignalEmitter<'_>,
        state: &str,
    ) -> zbus::Result<()>;
}

fn state_to_str(s: State) -> &'static str {
    match s {
        State::Init => "init",
        State::Ready => "ready",
        State::Opening => "opening",
        State::Streaming => "streaming",
    }
}

/// Spawn the D-Bus service on the session bus.
/// Returns the command receiver for the ATVV session to consume.
pub async fn serve(
    state_rx: watch::Receiver<State>,
    info: DaemonInfo,
) -> anyhow::Result<(mpsc::Receiver<ExternalCommand>, zbus::Connection)> {
    let (command_tx, command_rx) = mpsc::channel::<ExternalCommand>(16);
    let iface = DaemonInterface::new(command_tx, state_rx.clone(), info);

    let connection = zbus::connection::Builder::session()?
        .name("org.atvvoice")?
        .serve_at("/org/atvvoice/Daemon", iface)?
        .build()
        .await?;

    // Spawn a task to emit StateChanged signals when the state changes.
    let conn = connection.clone();
    tokio::spawn(async move {
        let mut rx = state_rx;
        let mut prev_state = *rx.borrow();
        while rx.changed().await.is_ok() {
            let new_state = *rx.borrow();
            if new_state != prev_state {
                prev_state = new_state;
                let object_server = conn.object_server();
                if let Ok(iface_ref) = object_server
                    .interface::<_, DaemonInterface>("/org/atvvoice/Daemon")
                    .await
                {
                    let _ = DaemonInterface::mic_state_changed(
                        iface_ref.signal_emitter(),
                        state_to_str(new_state),
                    )
                    .await;
                }
            }
        }
    });

    tracing::info!("D-Bus interface registered on session bus (org.atvvoice)");
    Ok((command_rx, connection))
}
