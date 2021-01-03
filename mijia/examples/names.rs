//! Example of how to subscribe to readings from one or more sensors.

use eyre::Report;
use eyre::WrapErr;
use mijia::{MijiaSession, SensorProps};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Duration;
use std::{io::stdin, process::exit};
use tokio::time;

const SCAN_DURATION: Duration = Duration::from_secs(5);
const SENSOR_NAMES_FILE: &str = "sensor-names.toml";

#[tokio::main]
async fn main() -> Result<(), Report> {
    pretty_env_logger::init();

    let excludes = get_known_sensors()?;
    println!("ignoring sensors: {:?}", excludes);

    let (_, session) = MijiaSession::new().await?;

    // Start scanning for Bluetooth devices, and wait a while for some to be discovered.
    session.bt_session.start_discovery().await?;
    time::sleep(SCAN_DURATION).await;

    // Get the list of sensors which are currently known, connect those which match the filter and
    // subscribe to readings.
    let sensors = session.get_sensors().await?;
    for sensor in sensors
        .iter()
        .filter(|sensor| should_include_sensor(sensor, &excludes))
    {
        println!("Connecting to {}", sensor.mac_address);
        if let Err(e) = session.bt_session.connect(&sensor.id).await {
            println!("Failed to connect to {}: {:?}", sensor.mac_address, e);

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(SENSOR_NAMES_FILE)?;
            writeln!(file, r#""{mac}" = "failed""#, mac = sensor.mac_address,)?;
            continue;
        }

        {
            let sensor = sensor.clone();

            tokio::task::spawn_blocking(move || {
                println!("What name do you want to use for {}?", sensor.mac_address);
                let mut name = String::new();
                stdin().read_line(&mut name)?;
                name.truncate(name.trim_end().len());

                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(SENSOR_NAMES_FILE)?;
                writeln!(
                    file,
                    r#""{mac}" = "{name}""#,
                    mac = sensor.mac_address,
                    name = name
                )?;
                println!(
                    r#"written: "{mac}" = "{name}""#,
                    mac = sensor.mac_address,
                    name = name
                );
                Ok::<_, Report>(())
            })
            .await??;
        }

        session
            .bt_session
            .disconnect(&sensor.id)
            .await
            .or_else(|_| Ok::<(), Report>(println!("disconnecting failed")))?;
    }

    Ok(())
}

fn get_known_sensors() -> Result<Vec<String>, Report> {
    let sensor_names_contents = std::fs::read_to_string(SENSOR_NAMES_FILE)
        .wrap_err_with(|| format!("Reading {}", SENSOR_NAMES_FILE))?;
    let names = toml::from_str::<HashMap<String, String>>(&sensor_names_contents)?;

    if names
        .keys()
        .any(|f| f.contains(|c: char| !(c.is_ascii_hexdigit() || c == ':')))
    {
        eprintln!("Invalid MAC addresses {:?}", names);
        exit(1);
    }

    Ok(names.keys().cloned().collect())
}

fn should_include_sensor(sensor: &SensorProps, excludes: &Vec<String>) -> bool {
    let mac = sensor.mac_address.to_string();
    !excludes.iter().any(|filter| mac.contains(filter))
}