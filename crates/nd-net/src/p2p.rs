//! Descoberta Wi-Fi Direct (P2P) via NetworkManager — base do Miracast/WFD.
//!
//! O NetworkManager expõe um device dedicado `p2p-dev-wlpXsY` com a interface
//! `org.freedesktop.NetworkManager.Device.WifiP2P` (`StartFind`/`StopFind` +
//! propriedade `Peers`). Cada peer com `WfdIEs` não-vazio é um **sink Miracast**
//! (uma TV/projetor em modo "Espelhamento de Tela").
//!
//! Indisponível sob Flatpak (barramento de sistema).

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

use nd_core::{NdError, Result};

const I_ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const I_IP4CONFIG: &str = "org.freedesktop.NetworkManager.IP4Config";

/// WFD Information Elements anunciando o PC como **fonte** Wi-Fi Display com o
/// servidor RTSP na porta 7236 (mesmos bytes do C de referência).
/// Subelemento 0 (Device Info), len 6: device-type=source, RTSP port=0x1c44=7236.
pub const WFD_SOURCE_IES: &[u8] = &[0x00, 0x00, 0x06, 0x00, 0x90, 0x1c, 0x44, 0x00, 0xc8];

/// Estado de uma conexão ativa do NetworkManager (`NMActiveConnectionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveState {
    Unknown,
    Activating,
    Activated,
    Deactivating,
    Deactivated,
}

impl From<u32> for ActiveState {
    fn from(v: u32) -> Self {
        match v {
            1 => ActiveState::Activating,
            2 => ActiveState::Activated,
            3 => ActiveState::Deactivating,
            4 => ActiveState::Deactivated,
            _ => ActiveState::Unknown,
        }
    }
}

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const I_NM: &str = "org.freedesktop.NetworkManager";
const I_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const I_WIFI_P2P: &str = "org.freedesktop.NetworkManager.Device.WifiP2P";
const I_PEER: &str = "org.freedesktop.NetworkManager.WifiP2PPeer";
/// `NM_DEVICE_TYPE_WIFI_P2P`.
const DEVICE_TYPE_WIFI_P2P: u32 = 30;

fn err<E: std::fmt::Display>(e: E) -> NdError {
    NdError::Network(e.to_string())
}

/// Um peer Wi-Fi Direct descoberto.
#[derive(Clone, Debug)]
pub struct P2pPeer {
    /// Caminho D-Bus (identificador estável).
    pub path: String,
    /// Nome amigável anunciado.
    pub name: String,
    /// Endereço MAC do peer.
    pub hw_address: String,
    /// `true` se anuncia Wi-Fi Display (é um sink Miracast).
    pub is_wfd: bool,
}

/// Handle para o device Wi-Fi P2P do NetworkManager.
pub struct P2pDevice {
    conn: Connection,
    path: OwnedObjectPath,
}

impl P2pDevice {
    /// Abre o barramento de sistema e localiza o device Wi-Fi P2P.
    pub async fn open() -> Result<Self> {
        let conn = Connection::system().await.map_err(err)?;
        let nm = Proxy::new(&conn, NM, NM_PATH, I_NM).await.map_err(err)?;
        let devices: Vec<OwnedObjectPath> = nm.call("GetAllDevices", &()).await.map_err(err)?;

        for dev in devices {
            let device = Proxy::new(&conn, NM, dev.as_str(), I_DEVICE)
                .await
                .map_err(err)?;
            let dtype: u32 = device.get_property("DeviceType").await.unwrap_or(0);
            if dtype == DEVICE_TYPE_WIFI_P2P {
                return Ok(Self { conn, path: dev });
            }
        }
        Err(NdError::Unsupported(
            "nenhum device Wi-Fi P2P no NetworkManager (placa sem suporte?)".into(),
        ))
    }

    async fn p2p(&self) -> Result<Proxy<'_>> {
        Proxy::new(&self.conn, NM, self.path.as_str(), I_WIFI_P2P)
            .await
            .map_err(err)
    }

    /// Inicia a varredura por peers Wi-Fi Direct.
    pub async fn start_find(&self) -> Result<()> {
        let options: HashMap<String, Value> = HashMap::new();
        self.p2p()
            .await?
            .call::<_, _, ()>("StartFind", &(options,))
            .await
            .map_err(err)
    }

    /// Interrompe a varredura.
    pub async fn stop_find(&self) -> Result<()> {
        self.p2p()
            .await?
            .call::<_, _, ()>("StopFind", &())
            .await
            .map_err(err)
    }

    /// Forma um grupo Wi-Fi Direct com o peer (sink Miracast), ativando uma
    /// conexão `wifi-p2p` com nós no papel de **fonte** WFD. Devolve o caminho
    /// da conexão ativa (acompanhar com [`active_state`](Self::active_state)).
    ///
    /// Porta do `nd-wfd-p2p-sink.c`: `AddAndActivateConnection2` com settings
    /// `connection`(wifi-p2p) + `wifi-p2p`(wfd-ies) + ipv4/ipv6 auto/never-default.
    pub async fn connect(&self, peer_path: &str) -> Result<OwnedObjectPath> {
        let nm = Proxy::new(&self.conn, NM, NM_PATH, I_NM).await.map_err(err)?;

        let mut connection: HashMap<String, Value> = HashMap::new();
        connection.insert("type".into(), Value::from("wifi-p2p"));
        if let Ok(user) = std::env::var("USER") {
            connection.insert("permissions".into(), Value::from(vec![format!("user:{user}:")]));
        }

        let mut wifi_p2p: HashMap<String, Value> = HashMap::new();
        wifi_p2p.insert("wfd-ies".into(), Value::from(WFD_SOURCE_IES.to_vec()));

        // Nunca rotear por esta conexão (é só o link P2P para o cast).
        let mut ipv4: HashMap<String, Value> = HashMap::new();
        ipv4.insert("method".into(), Value::from("auto"));
        ipv4.insert("never-default".into(), Value::from(true));

        let mut ipv6: HashMap<String, Value> = HashMap::new();
        ipv6.insert("method".into(), Value::from("auto"));
        ipv6.insert("never-default".into(), Value::from(true));
        ipv6.insert("may-fail".into(), Value::from(true));

        let mut settings: HashMap<String, HashMap<String, Value>> = HashMap::new();
        settings.insert("connection".into(), connection);
        settings.insert("wifi-p2p".into(), wifi_p2p);
        settings.insert("ipv4".into(), ipv4);
        settings.insert("ipv6".into(), ipv6);

        let mut options: HashMap<String, Value> = HashMap::new();
        options.insert("bind-activation".into(), Value::from("dbus-client"));
        options.insert("persist".into(), Value::from("volatile"));

        let peer = OwnedObjectPath::try_from(peer_path).map_err(err)?;

        let (_conn_path, active, _result): (
            OwnedObjectPath,
            OwnedObjectPath,
            HashMap<String, OwnedValue>,
        ) = nm
            .call(
                "AddAndActivateConnection2",
                &(settings, self.path.clone(), peer, options),
            )
            .await
            .map_err(err)?;

        Ok(active)
    }

    /// Lê os endereços IPv4 locais atribuídos a uma conexão ativa (nosso IP no
    /// link P2P — onde o servidor RTSP do WFD vai escutar, na Fase 3c).
    pub async fn addresses(&self, active: &OwnedObjectPath) -> Result<Vec<String>> {
        let ac = Proxy::new(&self.conn, NM, active.as_str(), I_ACTIVE)
            .await
            .map_err(err)?;
        let ip4: OwnedObjectPath = ac.get_property("Ip4Config").await.map_err(err)?;
        if ip4.as_str() == "/" {
            return Ok(Vec::new());
        }

        let ip4_proxy = Proxy::new(&self.conn, NM, ip4.as_str(), I_IP4CONFIG)
            .await
            .map_err(err)?;
        let data: Vec<HashMap<String, OwnedValue>> =
            ip4_proxy.get_property("AddressData").await.map_err(err)?;

        let mut addresses = Vec::new();
        for entry in &data {
            if let Some(value) = entry.get("address") {
                if let Ok(addr) = String::try_from(value.clone()) {
                    addresses.push(addr);
                }
            }
        }
        Ok(addresses)
    }

    /// Lê o estado de uma conexão ativa. Se o objeto sumiu (a conexão caiu —
    /// comum no driver Realtek), devolve `Deactivated` em vez de erro.
    pub async fn active_state(&self, active: &OwnedObjectPath) -> Result<ActiveState> {
        let proxy = match Proxy::new(&self.conn, NM, active.as_str(), I_ACTIVE).await {
            Ok(p) => p,
            Err(_) => return Ok(ActiveState::Deactivated),
        };
        match proxy.get_property::<u32>("State").await {
            Ok(state) => Ok(ActiveState::from(state)),
            Err(_) => Ok(ActiveState::Deactivated),
        }
    }

    /// Lê a lista atual de peers descobertos.
    pub async fn peers(&self) -> Result<Vec<P2pPeer>> {
        let p2p = self.p2p().await?;
        let paths: Vec<OwnedObjectPath> = p2p.get_property("Peers").await.map_err(err)?;

        let mut peers = Vec::with_capacity(paths.len());
        for path in paths {
            let peer = Proxy::new(&self.conn, NM, path.as_str(), I_PEER)
                .await
                .map_err(err)?;
            let name: String = peer.get_property("Name").await.unwrap_or_default();
            let hw_address: String = peer.get_property("HwAddress").await.unwrap_or_default();
            let wfd_ies: Vec<u8> = peer.get_property("WfdIEs").await.unwrap_or_default();
            peers.push(P2pPeer {
                path: path.to_string(),
                name,
                hw_address,
                is_wfd: !wfd_ies.is_empty(),
            });
        }
        Ok(peers)
    }
}
