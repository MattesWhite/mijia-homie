//! An async wrapper around the D-Bus interface of BlueZ, the Linux Bluetooth daemon. This provides
//! type-safe interfaces to a subset of the Bluetooth client (i.e. central, in Bluetooth
//! terminology) interfaces exposed by BlueZ, focussing on the Generic Attribute Profile (GATT) of
//! Bluetooth Low Energy (BLE).
//!
//! Start by creating a [`BluetoothSession`].
//!
//! [`BluetoothSession']: struct.BluetoothSession.html

mod bleuuid;
mod events;
mod introspect;
mod messagestream;

pub use self::bleuuid::{uuid_from_u16, uuid_from_u32, BleUuid};
pub use self::events::{AdapterEvent, BluetoothEvent, CharacteristicEvent, DeviceEvent};
use self::introspect::IntrospectParse;
use self::messagestream::MessageStream;
use bitflags::bitflags;
use bluez_generated::{
    OrgBluezAdapter1, OrgBluezDevice1, OrgBluezDevice1Properties, OrgBluezGattCharacteristic1,
    OrgBluezGattDescriptor1, OrgBluezGattService1, ORG_BLUEZ_ADAPTER1_NAME, ORG_BLUEZ_DEVICE1_NAME,
};
use dbus::arg::{cast, PropMap, RefArg, Variant};
use dbus::nonblock::stdintf::org_freedesktop_dbus::{Introspectable, ObjectManager, Properties};
use dbus::nonblock::{Proxy, SyncConnection};
use dbus::Path;
use dbus_tokio::connection::IOResourceError;
use futures::stream::{self, select_all, StreamExt};
use futures::{FutureExt, Stream};
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::fmt::{self, Debug, Display, Formatter};
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::task::JoinError;
use uuid::Uuid;

const DBUS_METHOD_CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// An error carrying out a Bluetooth operation.
#[derive(Debug, Error)]
pub enum BluetoothError {
    /// No Bluetooth adapters were found on the system.
    #[error("No Bluetooth adapters found.")]
    NoBluetoothAdapters,
    /// There was an error talking to the BlueZ daemon over D-Bus.
    #[error(transparent)]
    DbusError(#[from] dbus::Error),
    /// Error parsing XML for introspection.
    #[error("Error parsing XML for introspection: {0}")]
    XmlParseError(#[from] serde_xml_rs::Error),
    /// No service or characteristic was found for some UUID.
    #[error("Service or characteristic UUID {uuid} not found.")]
    UUIDNotFound { uuid: Uuid },
    /// Error parsing a UUID from a string.
    #[error("Error parsing UUID string: {0}")]
    UUIDParseError(#[from] uuid::Error),
    /// Error parsing a characteristic flag from a string.
    #[error("Invalid characteristic flag {0:?}")]
    FlagParseError(String),
    /// A required property of some device or other object was not found.
    #[error("Required property {0} missing.")]
    RequiredPropertyMissing(String),
}

/// Error type for futures representing tasks spawned by this crate.
#[derive(Debug, Error)]
pub enum SpawnError {
    #[error("D-Bus connection lost: {0}")]
    DbusConnectionLost(#[source] IOResourceError),
    #[error("Task failed: {0}")]
    Join(#[from] JoinError),
}

/// Opaque identifier for a Bluetooth adapter on the system.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AdapterId {
    object_path: Path<'static>,
}

impl AdapterId {
    fn new(object_path: &str) -> Self {
        Self {
            object_path: object_path.to_owned().into(),
        }
    }
}

impl Display for AdapterId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.object_path
                .to_string()
                .strip_prefix("/org/bluez/")
                .ok_or(fmt::Error)?
        )
    }
}

/// Opaque identifier for a Bluetooth device which the system knows about. This includes a reference
/// to which Bluetooth adapter it was discovered on, which means that any attempt to connect to it
/// will also happen from that adapter (in case the system has more than one).
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceId {
    object_path: Path<'static>,
}

impl DeviceId {
    fn new(object_path: &str) -> Self {
        Self {
            object_path: object_path.to_owned().into(),
        }
    }

    /// Get the ID of the Bluetooth adapter on which this device was discovered, e.g. `"hci0"`.
    pub fn adapter(&self) -> AdapterId {
        let index = self
            .object_path
            .rfind('/')
            .expect("DeviceId object_path must contain a slash.");
        AdapterId::new(&self.object_path[0..index])
    }
}

impl From<DeviceId> for Path<'static> {
    fn from(id: DeviceId) -> Self {
        id.object_path
    }
}

impl Display for DeviceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.object_path
                .to_string()
                .strip_prefix("/org/bluez/")
                .ok_or(fmt::Error)?
        )
    }
}

/// Opaque identifier for a GATT service on a Bluetooth device.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceId {
    object_path: Path<'static>,
}

impl ServiceId {
    fn new(object_path: &str) -> Self {
        Self {
            object_path: object_path.to_owned().into(),
        }
    }

    /// Get the ID of the device on which this service was advertised.
    pub fn device(&self) -> DeviceId {
        let index = self
            .object_path
            .rfind('/')
            .expect("ServiceId object_path must contain a slash.");
        DeviceId::new(&self.object_path[0..index])
    }
}

impl From<ServiceId> for Path<'static> {
    fn from(id: ServiceId) -> Self {
        id.object_path
    }
}

impl Display for ServiceId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.object_path
                .to_string()
                .strip_prefix("/org/bluez/")
                .ok_or(fmt::Error)?
        )
    }
}

/// Opaque identifier for a GATT characteristic on a Bluetooth device.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CharacteristicId {
    object_path: Path<'static>,
}

impl CharacteristicId {
    fn new(object_path: &str) -> Self {
        Self {
            object_path: object_path.to_owned().into(),
        }
    }

    /// Get the ID of the service on which this characteristic was advertised.
    pub fn service(&self) -> ServiceId {
        let index = self
            .object_path
            .rfind('/')
            .expect("CharacteristicId object_path must contain a slash.");
        ServiceId::new(&self.object_path[0..index])
    }
}

impl From<CharacteristicId> for Path<'static> {
    fn from(id: CharacteristicId) -> Self {
        id.object_path
    }
}

impl Display for CharacteristicId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.object_path
                .to_string()
                .strip_prefix("/org/bluez/")
                .ok_or(fmt::Error)?
        )
    }
}

/// Opaque identifier for a GATT characteristic descriptor on a Bluetooth device.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DescriptorId {
    object_path: Path<'static>,
}

impl DescriptorId {
    #[cfg(test)]
    fn new(object_path: &str) -> Self {
        Self {
            object_path: object_path.to_owned().into(),
        }
    }

    /// Get the ID of the characteristic on which this descriptor was advertised.
    pub fn characteristic(&self) -> CharacteristicId {
        let index = self
            .object_path
            .rfind('/')
            .expect("DescriptorId object_path must contain a slash.");
        CharacteristicId::new(&self.object_path[0..index])
    }
}

impl From<DescriptorId> for Path<'static> {
    fn from(id: DescriptorId) -> Self {
        id.object_path
    }
}

impl Display for DescriptorId {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            self.object_path
                .to_string()
                .strip_prefix("/org/bluez/")
                .ok_or(fmt::Error)?
        )
    }
}

/// MAC address of a Bluetooth device.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MacAddress(String);

impl Display for MacAddress {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An error parsing a MAC address from a string.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("Invalid MAC address")]
pub struct ParseMacAddressError();

impl FromStr for MacAddress {
    type Err = ParseMacAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let octets: Vec<_> = s.split(':').collect();
        if octets.len() != 6 {
            return Err(ParseMacAddressError());
        }
        for octet in octets {
            if octet.len() != 2 {
                return Err(ParseMacAddressError());
            }
            if !octet.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(ParseMacAddressError());
            }
        }
        Ok(MacAddress(s.to_uppercase()))
    }
}

/// Information about a Bluetooth device which was discovered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceInfo {
    /// An opaque identifier for the device, including a reference to which adapter it was
    /// discovered on. This can be used to connect to it.
    pub id: DeviceId,
    /// The MAC address of the device.
    pub mac_address: MacAddress,
    /// The human-readable name of the device, if available.
    pub name: Option<String>,
    /// The appearance of the device, as defined by GAP.
    pub appearance: Option<u16>,
    /// The GATT service UUIDs (if any) from the device's advertisement or service discovery.
    ///
    /// Note that service discovery only happens after a connection has been made to the device, but
    /// BlueZ may cache the list of services after it is disconnected.
    pub services: Vec<Uuid>,
    /// Whether the device is currently paired with the adapter.
    pub paired: bool,
    /// Whether the device is currently connected to the adapter.
    pub connected: bool,
    /// The Received Signal Strength Indicator of the device advertisement or inquiry.
    pub rssi: Option<i16>,
    /// Manufacturer-specific advertisement data, if any. The keys are 'manufacturer IDs'.
    pub manufacturer_data: HashMap<u16, Vec<u8>>,
    /// The GATT service data from the device's advertisement, if any. This is a map from the
    /// service UUID to its data.
    pub service_data: HashMap<Uuid, Vec<u8>>,
    /// Whether service discovery has finished for the device.
    pub services_resolved: bool,
}

impl DeviceInfo {
    fn from_properties(
        id: DeviceId,
        device_properties: OrgBluezDevice1Properties,
    ) -> Result<DeviceInfo, BluetoothError> {
        let mac_address = device_properties
            .address()
            .ok_or_else(|| BluetoothError::RequiredPropertyMissing("Address".to_string()))?;
        let services = get_services(device_properties);
        let manufacturer_data = get_manufacturer_data(device_properties).unwrap_or_default();
        let service_data = get_service_data(device_properties).unwrap_or_default();

        Ok(DeviceInfo {
            id,
            mac_address: MacAddress(mac_address.to_owned()),
            name: device_properties.name().cloned(),
            appearance: device_properties.appearance(),
            services,
            paired: device_properties
                .paired()
                .ok_or_else(|| BluetoothError::RequiredPropertyMissing("Paired".to_string()))?,
            connected: device_properties
                .connected()
                .ok_or_else(|| BluetoothError::RequiredPropertyMissing("Connected".to_string()))?,
            rssi: device_properties.rssi(),
            manufacturer_data,
            service_data,
            services_resolved: device_properties.services_resolved().ok_or_else(|| {
                BluetoothError::RequiredPropertyMissing("ServicesResolved".to_string())
            })?,
        })
    }
}

/// Information about a GATT service on a Bluetooth device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceInfo {
    /// An opaque identifier for the service on the device, including a reference to which adapter
    /// it was discovered on.
    pub id: ServiceId,
    /// The 128-bit UUID of the service.
    pub uuid: Uuid,
    /// Whether this GATT service is a primary service.
    pub primary: bool,
}

/// Information about a GATT characteristic on a Bluetooth device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CharacteristicInfo {
    /// An opaque identifier for the characteristic on the device, including a reference to which
    /// adapter it was discovered on.
    pub id: CharacteristicId,
    /// The 128-bit UUID of the characteristic.
    pub uuid: Uuid,
    /// The set of flags (a.k.a. properties) of the characteristic, defining how the characteristic
    /// can be used.
    pub flags: CharacteristicFlags,
}

bitflags! {
    /// The set of flags (a.k.a. properties) of a characteristic, defining how the characteristic
    /// can be used.
    pub struct CharacteristicFlags: u16 {
        const BROADCAST = 0x01;
        const READ = 0x02;
        const WRITE_WITHOUT_RESPONSE = 0x04;
        const WRITE = 0x08;
        const NOTIFY = 0x10;
        const INDICATE = 0x20;
        const SIGNED_WRITE = 0x40;
        const EXTENDED_PROPERTIES = 0x80;
        const RELIABLE_WRITE = 0x100;
        const WRITABLE_AUXILIARIES = 0x200;
        const ENCRYPT_READ = 0x400;
        const ENCRYPT_WRITE = 0x800;
        const ENCRYPT_AUTHENTICATED_READ = 0x1000;
        const ENCRYPT_AUTHENTICATED_WRITE = 0x2000;
        const AUTHORIZE = 0x4000;
    }
}

impl TryFrom<Vec<String>> for CharacteristicFlags {
    type Error = BluetoothError;

    fn try_from(value: Vec<String>) -> Result<Self, BluetoothError> {
        let mut flags = Self::empty();
        for flag_string in value {
            let flag = match flag_string.as_ref() {
                "broadcast" => Self::BROADCAST,
                "read" => Self::READ,
                "write-without-response" => Self::WRITE_WITHOUT_RESPONSE,
                "write" => Self::WRITE,
                "notify" => Self::NOTIFY,
                "indicate" => Self::INDICATE,
                "authenticated-signed-write" => Self::SIGNED_WRITE,
                "extended-properties" => Self::EXTENDED_PROPERTIES,
                "reliable-write" => Self::RELIABLE_WRITE,
                "writable-auxiliaries" => Self::WRITABLE_AUXILIARIES,
                "encrypt-read" => Self::ENCRYPT_READ,
                "encrypt-write" => Self::ENCRYPT_WRITE,
                "encrypt-authenticated-read" => Self::ENCRYPT_AUTHENTICATED_READ,
                "encrypt-authenticated-write" => Self::ENCRYPT_AUTHENTICATED_WRITE,
                "authorize" => Self::AUTHORIZE,
                _ => return Err(BluetoothError::FlagParseError(flag_string)),
            };
            flags.insert(flag);
        }
        Ok(flags)
    }
}

/// Information about a GATT descriptor on a Bluetooth device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DescriptorInfo {
    /// An opaque identifier for the descriptor on the device, including a reference to which
    /// adapter it was discovered on.
    pub id: DescriptorId,
    /// The 128-bit UUID of the descriptor.
    pub uuid: Uuid,
}

/// The type of transport to use for a scan.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Transport {
    /// Interleaved scan, both BLE and Bluetooth Classic (if they are both enabled on the adapter).
    Auto,
    /// BR/EDR inquiry, i.e. Bluetooth Classic.
    BrEdr,
    /// LE scan only.
    Le,
}

impl Transport {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::BrEdr => "bredr",
            Self::Le => "le",
        }
    }
}

impl Display for Transport {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A set of filter parameters for discovery. Parameters may be set to `None` to use the BlueZ
/// defaults.
///
/// If no parameters are set then there is a default filter on the RSSI values, where only values
/// which have changed more than a certain amount will be reported.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryFilter {
    /// If non-empty, only report devices which advertise at least one of these service UUIDs.
    pub service_uuids: Vec<Uuid>,
    /// Only report devices with RSSI values greater than the given threshold.
    pub rssi_threshold: Option<i16>,
    pub pathloss_threshold: Option<u16>,
    /// The type of scan.
    pub transport: Option<Transport>,
    /// Whether to include duplicate advertisements. If this is set to true then there will be an
    /// event whenever an advertisement containing manufacturer-specific data for a device is
    /// received.
    pub duplicate_data: Option<bool>,
    /// Whether to make the adapter discoverable while discovering.
    pub discoverable: Option<bool>,
    /// Only report devices whose address or name starts with the given pattern.
    pub pattern: Option<String>,
}

impl Into<PropMap> for &DiscoveryFilter {
    fn into(self) -> PropMap {
        let mut map: PropMap = HashMap::new();
        if !self.service_uuids.is_empty() {
            let uuids: Vec<String> = self.service_uuids.iter().map(Uuid::to_string).collect();
            map.insert("UUIDs".to_string(), Variant(Box::new(uuids)));
        }
        if let Some(rssi_threshold) = self.rssi_threshold {
            map.insert("RSSI".to_string(), Variant(Box::new(rssi_threshold)));
        }
        if let Some(pathloss_threshold) = self.pathloss_threshold {
            map.insert(
                "Pathloss".to_string(),
                Variant(Box::new(pathloss_threshold)),
            );
        }
        if let Some(transport) = self.transport {
            map.insert(
                "Transport".to_string(),
                Variant(Box::new(transport.to_string())),
            );
        }
        if let Some(duplicate_data) = self.duplicate_data {
            map.insert(
                "DuplicateData".to_string(),
                Variant(Box::new(duplicate_data)),
            );
        }
        if let Some(discoverable) = self.discoverable {
            map.insert("Discoverable".to_string(), Variant(Box::new(discoverable)));
        }
        if let Some(pattern) = &self.pattern {
            map.insert("Pattern".to_string(), Variant(Box::new(pattern.to_owned())));
        }
        map
    }
}

/// A connection to the Bluetooth daemon. This can be cheaply cloned and passed around to be used
/// from different places. It is the main entry point to the library.
#[derive(Clone)]
pub struct BluetoothSession {
    pub connection: Arc<SyncConnection>,
}

impl Debug for BluetoothSession {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "BluetoothSession")
    }
}

impl BluetoothSession {
    /// Establish a new D-Bus connection to communicate with BlueZ.
    ///
    /// Returns a tuple of (join handle, Self).
    /// If the join handle ever completes then you're in trouble and should
    /// probably restart the process.
    pub async fn new(
    ) -> Result<(impl Future<Output = Result<(), SpawnError>>, Self), BluetoothError> {
        // Connect to the D-Bus system bus (this is blocking, unfortunately).
        let (dbus_resource, connection) = dbus_tokio::connection::new_system_sync()?;
        // The resource is a task that should be spawned onto a tokio compatible
        // reactor ASAP. If the resource ever finishes, you lost connection to D-Bus.
        let dbus_handle = tokio::spawn(async {
            let err = dbus_resource.await;
            Err(SpawnError::DbusConnectionLost(err))
        });
        Ok((
            dbus_handle.map(|res| Ok(res??)),
            BluetoothSession { connection },
        ))
    }

    /// Power on all Bluetooth adapters, set the given discovery filter, and then start scanning for
    /// devices.
    ///
    /// Note that BlueZ combines discovery filters from all clients and sends events matching any
    /// filter to all clients, so you may receive unexpected discovery events if there are other
    /// clients on the system using Bluetooth as well.
    ///
    /// In most common cases, `DiscoveryFilter::default()` is fine.
    pub async fn start_discovery(
        &self,
        discovery_filter: &DiscoveryFilter,
    ) -> Result<(), BluetoothError> {
        let adapters = self.get_adapters().await?;
        if adapters.is_empty() {
            return Err(BluetoothError::NoBluetoothAdapters);
        }

        for adapter_id in adapters {
            log::trace!("Starting discovery on adapter {}", adapter_id);
            let adapter = self.adapter(&adapter_id);
            adapter.set_powered(true).await?;
            adapter
                .set_discovery_filter(discovery_filter.into())
                .await?;
            adapter
                .start_discovery()
                .await
                .unwrap_or_else(|err| println!("starting discovery failed {:?}", err));
        }
        Ok(())
    }

    /// Stop scanning for devices on all Bluetooth adapters.
    pub async fn stop_discovery(&self) -> Result<(), BluetoothError> {
        let adapters = self.get_adapters().await?;
        if adapters.is_empty() {
            return Err(BluetoothError::NoBluetoothAdapters);
        }

        for adapter_id in adapters {
            let adapter = self.adapter(&adapter_id);
            adapter.stop_discovery().await?;
        }

        Ok(())
    }

    /// Get a list of all Bluetooth adapters on the system.
    async fn get_adapters(&self) -> Result<Vec<AdapterId>, dbus::Error> {
        let bluez_root = Proxy::new(
            "org.bluez",
            "/",
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        // TODO: See whether there is a way to do this with introspection instead, rather than
        // getting lots of objects we don't care about.
        let tree = bluez_root.get_managed_objects().await?;
        Ok(tree
            .into_iter()
            .filter_map(|(object_path, interfaces)| {
                interfaces
                    .get(ORG_BLUEZ_ADAPTER1_NAME)
                    .map(|_| AdapterId { object_path })
            })
            .collect())
    }

    /// Get a list of all Bluetooth devices which have been discovered so far.
    pub async fn get_devices(&self) -> Result<Vec<DeviceInfo>, BluetoothError> {
        let bluez_root = Proxy::new(
            "org.bluez",
            "/",
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        );
        let tree = bluez_root.get_managed_objects().await?;

        let devices = tree
            .into_iter()
            .filter_map(|(object_path, interfaces)| {
                let device_properties = OrgBluezDevice1Properties::from_interfaces(&interfaces)?;
                DeviceInfo::from_properties(DeviceId { object_path }, device_properties).ok()
            })
            .collect();
        Ok(devices)
    }

    /// Get a list of all GATT services which the given Bluetooth device offers.
    ///
    /// Note that this won't be filled in until the device is connected.
    pub async fn get_services(
        &self,
        device: &DeviceId,
    ) -> Result<Vec<ServiceInfo>, BluetoothError> {
        let device_node = self.device(device).introspect_parse().await?;
        let mut services = vec![];
        for subnode in device_node.nodes {
            let subnode_name = subnode.name.as_ref().unwrap();
            // Service paths are always of the form
            // /org/bluez/{hci0,hci1,...}/dev_XX_XX_XX_XX_XX_XX/serviceXXXX
            if subnode_name.starts_with("service") {
                let service_id = ServiceId {
                    object_path: format!("{}/{}", device.object_path, subnode_name).into(),
                };
                let service = self.service(&service_id);
                let uuid = Uuid::parse_str(&service.uuid().await?)?;
                let primary = service.primary().await?;
                services.push(ServiceInfo {
                    id: service_id,
                    uuid,
                    primary,
                });
            }
        }
        Ok(services)
    }

    /// Get a list of all characteristics on the given GATT service.
    pub async fn get_characteristics(
        &self,
        service: &ServiceId,
    ) -> Result<Vec<CharacteristicInfo>, BluetoothError> {
        let service_node = self.service(service).introspect_parse().await?;
        let mut characteristics = vec![];
        for subnode in service_node.nodes {
            let subnode_name = subnode.name.as_ref().unwrap();
            // Characteristic paths are always of the form
            // /org/bluez/{hci0,hci1,...}/dev_XX_XX_XX_XX_XX_XX/serviceXXXX/charYYYY
            if subnode_name.starts_with("char") {
                let characteristic_id = CharacteristicId {
                    object_path: format!("{}/{}", service.object_path, subnode_name).into(),
                };
                let characteristic = self.characteristic(&characteristic_id);
                let uuid = Uuid::parse_str(&characteristic.uuid().await?)?;
                let flags = characteristic.flags().await?;
                characteristics.push(CharacteristicInfo {
                    id: characteristic_id,
                    uuid,
                    flags: flags.try_into()?,
                });
            }
        }
        Ok(characteristics)
    }

    /// Get a list of all descriptors on the given GATT characteristic.
    pub async fn get_descriptors(
        &self,
        characteristic: &CharacteristicId,
    ) -> Result<Vec<DescriptorInfo>, BluetoothError> {
        let characteristic_node = self
            .characteristic(characteristic)
            .introspect_parse()
            .await?;
        let mut descriptors = vec![];
        for subnode in characteristic_node.nodes {
            let subnode_name = subnode.name.as_ref().unwrap();
            // Service paths are always of the form
            // /org/bluez/{hci0,hci1,...}/dev_XX_XX_XX_XX_XX_XX/serviceXXXX/charYYYY/descZZZZ
            if subnode_name.starts_with("desc") {
                let descriptor_id = DescriptorId {
                    object_path: format!("{}/{}", characteristic.object_path, subnode_name).into(),
                };
                let uuid = Uuid::parse_str(&self.descriptor(&descriptor_id).uuid().await?)?;
                descriptors.push(DescriptorInfo {
                    id: descriptor_id,
                    uuid,
                });
            }
        }
        Ok(descriptors)
    }

    /// Find a GATT service with the given UUID advertised by the given device, if any.
    ///
    /// Note that this generally won't work until the device is connected.
    pub async fn get_service_by_uuid(
        &self,
        device: &DeviceId,
        uuid: Uuid,
    ) -> Result<ServiceInfo, BluetoothError> {
        let services = self.get_services(device).await?;
        services
            .into_iter()
            .find(|service_info| service_info.uuid == uuid)
            .ok_or(BluetoothError::UUIDNotFound { uuid })
    }

    /// Find a characteristic with the given UUID as part of the given GATT service advertised by a
    /// device, if there is any.
    pub async fn get_characteristic_by_uuid(
        &self,
        service: &ServiceId,
        uuid: Uuid,
    ) -> Result<CharacteristicInfo, BluetoothError> {
        let characteristics = self.get_characteristics(service).await?;
        characteristics
            .into_iter()
            .find(|characteristic_info| characteristic_info.uuid == uuid)
            .ok_or(BluetoothError::UUIDNotFound { uuid })
    }

    /// Convenience method to get a GATT charactacteristic with the given UUID advertised by a
    /// device as part of the given service.
    ///
    /// This is equivalent to calling `get_service_by_uuid` and then `get_characteristic_by_uuid`.
    pub async fn get_service_characteristic_by_uuid(
        &self,
        device: &DeviceId,
        service_uuid: Uuid,
        characteristic_uuid: Uuid,
    ) -> Result<CharacteristicInfo, BluetoothError> {
        let service = self.get_service_by_uuid(device, service_uuid).await?;
        self.get_characteristic_by_uuid(&service.id, characteristic_uuid)
            .await
    }

    /// Get information about the given Bluetooth device.
    pub async fn get_device_info(&self, id: &DeviceId) -> Result<DeviceInfo, BluetoothError> {
        let device = self.device(&id);
        let properties = device.get_all(ORG_BLUEZ_DEVICE1_NAME).await?;
        DeviceInfo::from_properties(id.to_owned(), OrgBluezDevice1Properties(&properties))
    }

    /// Get information about the given GATT service.
    pub async fn get_service_info(&self, id: &ServiceId) -> Result<ServiceInfo, BluetoothError> {
        let service = self.service(&id);
        let uuid = Uuid::parse_str(&service.uuid().await?)?;
        let primary = service.primary().await?;
        Ok(ServiceInfo {
            id: id.to_owned(),
            uuid,
            primary,
        })
    }

    /// Get information about the given GATT characteristic.
    pub async fn get_characteristic_info(
        &self,
        id: &CharacteristicId,
    ) -> Result<CharacteristicInfo, BluetoothError> {
        let characteristic = self.characteristic(&id);
        let uuid = Uuid::parse_str(&characteristic.uuid().await?)?;
        let flags = characteristic.flags().await?;
        Ok(CharacteristicInfo {
            id: id.to_owned(),
            uuid,
            flags: flags.try_into()?,
        })
    }

    /// Get information about the given GATT descriptor.
    pub async fn get_descriptor_info(
        &self,
        id: &DescriptorId,
    ) -> Result<DescriptorInfo, BluetoothError> {
        let uuid = Uuid::parse_str(&self.descriptor(&id).uuid().await?)?;
        Ok(DescriptorInfo {
            id: id.to_owned(),
            uuid,
        })
    }

    fn adapter(&self, id: &AdapterId) -> impl OrgBluezAdapter1 + Introspectable + Properties {
        Proxy::new(
            "org.bluez",
            id.object_path.to_owned(),
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        )
    }

    fn device(&self, id: &DeviceId) -> impl OrgBluezDevice1 + Introspectable + Properties {
        Proxy::new(
            "org.bluez",
            id.object_path.to_owned(),
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        )
    }

    fn service(&self, id: &ServiceId) -> impl OrgBluezGattService1 + Introspectable + Properties {
        Proxy::new(
            "org.bluez",
            id.object_path.to_owned(),
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        )
    }

    fn characteristic(
        &self,
        id: &CharacteristicId,
    ) -> impl OrgBluezGattCharacteristic1 + Introspectable + Properties {
        Proxy::new(
            "org.bluez",
            id.object_path.to_owned(),
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        )
    }

    fn descriptor(
        &self,
        id: &DescriptorId,
    ) -> impl OrgBluezGattDescriptor1 + Introspectable + Properties {
        Proxy::new(
            "org.bluez",
            id.object_path.to_owned(),
            DBUS_METHOD_CALL_TIMEOUT,
            self.connection.clone(),
        )
    }

    /// Connect to the given Bluetooth device.
    pub async fn connect(&self, id: &DeviceId) -> Result<(), BluetoothError> {
        Ok(self.device(id).connect().await?)
    }

    /// Disconnect from the given Bluetooth device.
    pub async fn disconnect(&self, id: &DeviceId) -> Result<(), BluetoothError> {
        Ok(self.device(id).disconnect().await?)
    }

    /// Read the value of the given GATT characteristic.
    pub async fn read_characteristic_value(
        &self,
        id: &CharacteristicId,
    ) -> Result<Vec<u8>, BluetoothError> {
        let characteristic = self.characteristic(id);
        Ok(characteristic.read_value(HashMap::new()).await?)
    }

    /// Write the given value to the given GATT characteristic.
    pub async fn write_characteristic_value(
        &self,
        id: &CharacteristicId,
        value: impl Into<Vec<u8>>,
    ) -> Result<(), BluetoothError> {
        let characteristic = self.characteristic(id);
        Ok(characteristic
            .write_value(value.into(), HashMap::new())
            .await?)
    }

    /// Read the value of the given GATT descriptor.
    pub async fn read_descriptor_value(
        &self,
        id: &DescriptorId,
    ) -> Result<Vec<u8>, BluetoothError> {
        let descriptor = self.descriptor(id);
        Ok(descriptor.read_value(HashMap::new()).await?)
    }

    /// Write the given value to the given GATT descriptor.
    pub async fn write_descriptor_value(
        &self,
        id: &DescriptorId,
        value: impl Into<Vec<u8>>,
    ) -> Result<(), BluetoothError> {
        let descriptor = self.descriptor(id);
        Ok(descriptor.write_value(value.into(), HashMap::new()).await?)
    }

    /// Start notifications on the given GATT characteristic.
    pub async fn start_notify(&self, id: &CharacteristicId) -> Result<(), BluetoothError> {
        let characteristic = self.characteristic(id);
        characteristic.start_notify().await?;
        Ok(())
    }

    /// Stop notifications on the given GATT characteristic.
    pub async fn stop_notify(&self, id: &CharacteristicId) -> Result<(), BluetoothError> {
        let characteristic = self.characteristic(id);
        characteristic.stop_notify().await?;
        Ok(())
    }

    /// Get a stream of events for all devices.
    pub async fn event_stream(&self) -> Result<impl Stream<Item = BluetoothEvent>, BluetoothError> {
        self.filtered_event_stream(None::<&DeviceId>).await
    }

    /// Get a stream of events for a particular device. This includes events for all its
    /// characteristics.
    pub async fn device_event_stream(
        &self,
        device: &DeviceId,
    ) -> Result<impl Stream<Item = BluetoothEvent>, BluetoothError> {
        self.filtered_event_stream(Some(device)).await
    }

    /// Get a stream of events for a particular characteristic of a device.
    pub async fn characteristic_event_stream(
        &self,
        characteristic: &CharacteristicId,
    ) -> Result<impl Stream<Item = BluetoothEvent>, BluetoothError> {
        self.filtered_event_stream(Some(characteristic)).await
    }

    async fn filtered_event_stream(
        &self,
        object: Option<&(impl Into<Path<'static>> + Clone)>,
    ) -> Result<impl Stream<Item = BluetoothEvent>, BluetoothError> {
        let mut message_streams = vec![];
        for match_rule in BluetoothEvent::match_rules(object.cloned()) {
            let msg_match = self.connection.add_match(match_rule).await?;
            message_streams.push(MessageStream::new(msg_match, self.connection.clone()));
        }
        Ok(select_all(message_streams)
            .flat_map(|message| stream::iter(BluetoothEvent::message_to_events(message))))
    }
}

fn get_manufacturer_data(
    device_properties: OrgBluezDevice1Properties,
) -> Option<HashMap<u16, Vec<u8>>> {
    Some(convert_manufacturer_data(
        device_properties.manufacturer_data()?,
    ))
}

pub(crate) fn convert_manufacturer_data(
    data: &HashMap<u16, Variant<Box<dyn RefArg>>>,
) -> HashMap<u16, Vec<u8>> {
    data.iter()
        .filter_map(|(&k, v)| {
            if let Some(v) = cast::<Vec<u8>>(&v.0) {
                Some((k, v.to_owned()))
            } else {
                log::warn!("Manufacturer data had wrong type: {:?}", &v.0);
                None
            }
        })
        .collect()
}

fn get_service_data(
    device_properties: OrgBluezDevice1Properties,
) -> Option<HashMap<Uuid, Vec<u8>>> {
    // UUIDs don't get populated until we connect. Use:
    //     "ServiceData": Variant(InternalDict { data: [
    //         ("0000fe95-0000-1000-8000-00805f9b34fb", Variant([48, 88, 91, 5, 1, 23, 33, 215, 56, 193, 164, 40, 1, 0])
    //     )], outer_sig: Signature("a{sv}") })
    // instead.
    Some(
        device_properties
            .service_data()?
            .iter()
            .filter_map(|(k, v)| match Uuid::parse_str(k) {
                Ok(uuid) => {
                    if let Some(v) = cast::<Vec<u8>>(&v.0) {
                        Some((uuid, v.to_owned()))
                    } else {
                        log::warn!("Service data had wrong type: {:?}", &v.0);
                        None
                    }
                }
                Err(err) => {
                    log::warn!("Error parsing service data UUID: {}", err);
                    None
                }
            })
            .collect(),
    )
}

fn get_services(device_properties: OrgBluezDevice1Properties) -> Vec<Uuid> {
    if let Some(uuids) = device_properties.uuids() {
        uuids
            .iter()
            .filter_map(|uuid| {
                Uuid::parse_str(uuid)
                    .map_err(|err| {
                        log::warn!("Error parsing service data UUID: {}", err);
                        err
                    })
                    .ok()
            })
            .collect()
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_adapter() {
        let adapter_id = AdapterId::new("/org/bluez/hci0");
        let device_id = DeviceId::new("/org/bluez/hci0/dev_11_22_33_44_55_66");
        assert_eq!(device_id.adapter(), adapter_id);
    }

    #[test]
    fn service_device() {
        let device_id = DeviceId::new("/org/bluez/hci0/dev_11_22_33_44_55_66");
        let service_id = ServiceId::new("/org/bluez/hci0/dev_11_22_33_44_55_66/service0022");
        assert_eq!(service_id.device(), device_id);
    }

    #[test]
    fn characteristic_service() {
        let service_id = ServiceId::new("/org/bluez/hci0/dev_11_22_33_44_55_66/service0022");
        let characteristic_id =
            CharacteristicId::new("/org/bluez/hci0/dev_11_22_33_44_55_66/service0022/char0033");
        assert_eq!(characteristic_id.service(), service_id);
    }

    #[test]
    fn descriptor_characteristic() {
        let characteristic_id =
            CharacteristicId::new("/org/bluez/hci0/dev_11_22_33_44_55_66/service0022/char0033");
        let descriptor_id = DescriptorId::new(
            "/org/bluez/hci0/dev_11_22_33_44_55_66/service0022/char0033/desc0034",
        );
        assert_eq!(descriptor_id.characteristic(), characteristic_id);
    }

    #[test]
    fn parse_flags() {
        let flags: CharacteristicFlags = vec!["read".to_string(), "encrypt-write".to_string()]
            .try_into()
            .unwrap();
        assert_eq!(
            flags,
            CharacteristicFlags::READ | CharacteristicFlags::ENCRYPT_WRITE
        )
    }

    #[test]
    fn parse_flags_fail() {
        let flags: Result<CharacteristicFlags, BluetoothError> =
            vec!["read".to_string(), "invalid flag".to_string()].try_into();
        assert!(
            matches!(flags, Err(BluetoothError::FlagParseError(string)) if string == "invalid flag")
        );
    }

    #[test]
    fn service_data() {
        let uuid = uuid_from_u32(0x11223344);
        let mut service_data: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
        service_data.insert(uuid.to_string(), Variant(Box::new(vec![1u8, 2, 3])));
        let mut device_properties: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
        device_properties.insert("ServiceData".to_string(), Variant(Box::new(service_data)));

        let mut expected_service_data = HashMap::new();
        expected_service_data.insert(uuid, vec![1u8, 2, 3]);

        assert_eq!(
            get_service_data(OrgBluezDevice1Properties(&device_properties)),
            Some(expected_service_data)
        );
    }

    #[test]
    fn manufacturer_data() {
        let manufacturer_id = 0x1122;
        let mut manufacturer_data: HashMap<u16, Variant<Box<dyn RefArg>>> = HashMap::new();
        manufacturer_data.insert(manufacturer_id, Variant(Box::new(vec![1u8, 2, 3])));
        let mut device_properties: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
        device_properties.insert(
            "ManufacturerData".to_string(),
            Variant(Box::new(manufacturer_data)),
        );

        let mut expected_manufacturer_data = HashMap::new();
        expected_manufacturer_data.insert(manufacturer_id, vec![1u8, 2, 3]);

        assert_eq!(
            get_manufacturer_data(OrgBluezDevice1Properties(&device_properties)),
            Some(expected_manufacturer_data)
        );
    }

    #[test]
    fn device_info_minimal() {
        let id = DeviceId::new("/org/bluez/hci0/dev_11_22_33_44_55_66");
        let mut device_properties: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
        device_properties.insert(
            "Address".to_string(),
            Variant(Box::new("00:11:22:33:44:55".to_string())),
        );
        device_properties.insert("Paired".to_string(), Variant(Box::new(false)));
        device_properties.insert("Connected".to_string(), Variant(Box::new(false)));
        device_properties.insert("ServicesResolved".to_string(), Variant(Box::new(false)));

        let device =
            DeviceInfo::from_properties(id.clone(), OrgBluezDevice1Properties(&device_properties))
                .unwrap();
        assert_eq!(
            device,
            DeviceInfo {
                id,
                mac_address: MacAddress("00:11:22:33:44:55".to_string()),
                name: None,
                appearance: None,
                services: vec![],
                paired: false,
                connected: false,
                rssi: None,
                manufacturer_data: HashMap::new(),
                service_data: HashMap::new(),
                services_resolved: false,
            }
        )
    }

    #[test]
    fn get_services_none() {
        let device_properties: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();

        assert_eq!(
            get_services(OrgBluezDevice1Properties(&device_properties)),
            vec![]
        )
    }

    #[test]
    fn get_services_some() {
        let uuid = uuid_from_u32(0x11223344);
        let uuids = vec![uuid.to_string()];
        let mut device_properties: HashMap<String, Variant<Box<dyn RefArg>>> = HashMap::new();
        device_properties.insert("UUIDs".to_string(), Variant(Box::new(uuids)));

        assert_eq!(
            get_services(OrgBluezDevice1Properties(&device_properties)),
            vec![uuid]
        )
    }
}
