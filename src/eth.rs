//! Ethernet, for the ESP32-P4, which has no radio: the chip's EMAC with an IP101 PHY over
//! RMII. The pins are ESP-IDF's own P4 defaults (`ETH_ESP32_EMAC_DEFAULT_CONFIG`), which
//! are those of Espressif's P4 Function EV board; esp-idf-svc drives only the original
//! ESP32's EMAC, so the driver is installed here. Addresses come by DHCPv4 and SLAAC, as on
//! Wi-Fi, and there is nothing to provision.

use core::ffi::{c_void, CStr};
use core::ptr;
use std::ffi::CString;

use esp_idf_svc::eventloop::{EspSubscription, EspSystemEventLoop, System};
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::netif::{EspNetif, IpEvent, NetifConfiguration};
use esp_idf_svc::sys::{self, esp};
use log::info;

pub const NETIF_KEY: &CStr = c"ETH_DEF";

const MDC: i32 = 31;
const MDIO: i32 = 52;
/// The PHY supplies the 50 MHz RMII clock.
const RMII_CLOCK_IN: i32 = 50;
const TX_EN: i32 = 49;
const TXD0: i32 = 34;
const TXD1: i32 = 35;
const CRS_DV: i32 = 28;
const RXD0: i32 = 29;
const RXD1: i32 = 30;
const PHY_RESET: i32 = 51;
const PHY_ADDRESS: i32 = 1;

/// The interface and what reports on it; dropping it would stop neither the driver nor the
/// netif, which run for the life of the firmware.
pub struct Ethernet {
    _netif: EspNetif,
    _addresses: EspSubscription<'static, System>,
}

/// `thingino-backpack-` and the last two octets of the Ethernet MAC.
pub fn default_hostname() -> String {
    let mut mac = [0u8; 6];
    // SAFETY: `mac` is the six bytes the call writes.
    unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_ETH) };
    format!("thingino-backpack-{:02x}{:02x}", mac[4], mac[5])
}

pub fn start(sysloop: EspSystemEventLoop, hostname: &str) -> Result<Ethernet, String> {
    // SAFETY: all-zero is valid for this plain C configuration; every field that matters is
    // set below.
    let mut emac: sys::eth_esp32_emac_config_t = unsafe { core::mem::zeroed() };
    emac.__bindgen_anon_1.smi_gpio = sys::emac_esp_smi_gpio_config_t {
        mdc_num: MDC,
        mdio_num: MDIO,
    };
    emac.interface = sys::eth_data_interface_t_EMAC_DATA_INTERFACE_RMII;
    emac.clock_config.rmii.clock_mode = sys::emac_rmii_clock_mode_t_EMAC_CLK_EXT_IN;
    emac.clock_config.rmii.clock_gpio = RMII_CLOCK_IN;
    emac.dma_burst_len = sys::eth_mac_dma_burst_len_t_ETH_DMA_BURST_LEN_32;
    emac.emac_dataif_gpio.rmii = sys::eth_mac_rmii_gpio_config_t {
        tx_en_num: TX_EN,
        txd0_num: TXD0,
        txd1_num: TXD1,
        crs_dv_num: CRS_DV,
        rxd0_num: RXD0,
        rxd1_num: RXD1,
    };
    emac.clock_config_out_in.rmii.clock_mode = sys::emac_rmii_clock_mode_t_EMAC_CLK_EXT_IN;
    emac.clock_config_out_in.rmii.clock_gpio = -1;
    let mac_config = sys::eth_mac_config_t {
        sw_reset_timeout_ms: 100,
        rx_task_stack_size: 4096,
        rx_task_prio: 15,
        flags: 0,
    };
    // SAFETY: both configurations outlive the call, which copies them.
    let mac = unsafe { sys::esp_eth_mac_new_esp32(&emac, &mac_config) };
    if mac.is_null() {
        return Err("Ethernet: the EMAC would not start".into());
    }
    let phy_config = sys::eth_phy_config_t {
        phy_addr: PHY_ADDRESS,
        reset_timeout_ms: 100,
        autonego_timeout_ms: 4000,
        reset_gpio_num: PHY_RESET,
        hw_reset_assert_time_us: 0,
        post_hw_reset_delay_ms: 0,
    };
    // SAFETY: as above.
    let phy = unsafe { sys::esp_eth_phy_new_ip101(&phy_config) };
    if phy.is_null() {
        return Err("Ethernet: no IP101 PHY".into());
    }
    // SAFETY: as above; the rest of the configuration is optional hooks left empty.
    let mut config: sys::esp_eth_config_t = unsafe { core::mem::zeroed() };
    config.mac = mac;
    config.phy = phy;
    config.check_link_period_ms = 2000;
    let mut driver = ptr::null_mut();
    // SAFETY: `config` and `driver` outlive the call.
    esp!(unsafe { sys::esp_eth_driver_install(&config, &mut driver) }).map_err(|err| format!("Ethernet: {err}"))?;

    // SLAAC and MLDv6 reports, as the Wi-Fi station has them (see wifi.rs).
    let default = NetifConfiguration::eth_default_client();
    let netif = EspNetif::new_with_conf(&NetifConfiguration {
        flags: default.flags
            | sys::esp_netif_flags_ESP_NETIF_FLAG_IPV6_AUTOCONFIG_ENABLED
            | sys::esp_netif_flags_ESP_NETIF_FLAG_MLDV6_REPORT,
        ..default
    })
    .map_err(|err| format!("Ethernet: {err}"))?;
    // SAFETY: the driver is installed; the glue lives as long as the driver, for ever.
    let glue = unsafe { sys::esp_eth_new_netif_glue(driver) };
    // SAFETY: the netif and the glue are both live.
    esp!(unsafe { sys::esp_netif_attach(netif.handle(), glue.cast()) }).map_err(|err| format!("Ethernet: {err}"))?;
    let name = CString::new(hostname).map_err(|_| "the hostname has a NUL".to_owned())?;
    // SAFETY: the netif is live and the call copies the name.
    unsafe { sys::esp_netif_set_hostname(netif.handle(), name.as_ptr()) };

    // SAFETY: the handler only reads its argument, the netif, which lives for ever.
    esp!(unsafe {
        sys::esp_event_handler_register(
            sys::ETH_EVENT,
            sys::eth_event_t_ETHERNET_EVENT_CONNECTED as i32,
            Some(link_up),
            netif.handle().cast(),
        )
    })
    .map_err(|err| format!("Ethernet: {err}"))?;
    let addresses = sysloop
        .subscribe::<IpEvent, _>(|event| match event {
            IpEvent::DhcpIpAssigned(got) => info!("eth: address {}", got.ip()),
            IpEvent::DhcpIp6Assigned(got) => info!("eth: address {}", got.addr()),
            _ => {}
        })
        .map_err(|err| format!("Ethernet: {err}"))?;
    // SAFETY: the driver is installed and attached.
    esp!(unsafe { sys::esp_eth_start(driver) }).map_err(|err| format!("Ethernet: {err}"))?;
    Ok(Ethernet {
        _netif: netif,
        _addresses: addresses,
    })
}

/// The netif drops its IPv6 addresses whenever the link goes down.
unsafe extern "C" fn link_up(netif: *mut c_void, _base: sys::esp_event_base_t, _id: i32, _data: *mut c_void) {
    info!("eth: link up");
    // SAFETY: `netif` is the handle registered with this handler, live for ever.
    unsafe { sys::esp_netif_create_ip6_linklocal(netif.cast()) };
}
