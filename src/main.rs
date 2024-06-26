use std::{
  env,
  net::TcpStream,
  sync::{Arc, RwLock, Weak},
  thread,
  time::Duration,
};

use anyhow::Context;
use either::Either;
use hex::FromHex;
use serde_json::json;
use serialport::{DataBits, Parity, StopBits};

use dlms_cosem::{Data, DateTime, Dlms, ObisCode};
use smart_meter::SmartMeter;
use webthing::{server::ActionGenerator, Action, BaseProperty, BaseThing, Thing, ThingsType, WebThingServer};

struct Generator;

impl ActionGenerator for Generator {
  fn generate(
    &self,
    _thing: Weak<RwLock<Box<dyn Thing>>>,
    _name: String,
    _input: Option<&serde_json::Value>,
  ) -> Option<Box<dyn Action>> {
    None
  }
}

#[actix_rt::main]
async fn main() -> anyhow::Result<()> {
  env_logger::init();

  let url_or_path = env::var("SERIAL_PORT").unwrap_or_else(|_| "/dev/serial0".into());
  let key = env::var("KEY").expect("No key provided");
  let key = <[u8; 16]>::from_hex(key).expect("Invalid key format");
  let port = env::var("PORT").map(|s| s.parse::<u16>().expect("Port is invalid")).unwrap_or(8888);

  let stream = if url_or_path.contains(':') {
    log::info!("Connecting to serial device {url_or_path}…");
    Either::Left(TcpStream::connect(&url_or_path).with_context(|| format!("Failed to connect to {url_or_path}"))?)
  } else {
    log::info!("Opening serial device {url_or_path}…");
    let mut serial_port = serialport::new(&url_or_path, 2400)
      .parity(Parity::Even)
      .data_bits(DataBits::Eight)
      .stop_bits(StopBits::One)
      .timeout(Duration::from_secs(30))
      .open_native()
      .with_context(|| format!("Failed to open {url_or_path}"))?;

    serial_port.set_exclusive(true).with_context(|| format!("Failed to get exclusive access to {url_or_path}"))?;

    Either::Right(serial_port)
  };

  let dlms = Dlms::new(key);

  let smart_meter = SmartMeter::new(stream, dlms);

  let mut smart_meter = smart_meter.map(|res| match res {
    Ok(mut obis) => {
      let convert_date_time = |value| match value {
        Data::OctetString(value) => Data::DateTime(DateTime::parse(&value).unwrap().1),
        value => value,
      };
      obis.convert(&ObisCode::new(0, 0, 1, 0, 0, 255), convert_date_time);

      let convert_string = |value| match value {
        Data::OctetString(value) => Data::Utf8String(String::from_utf8(value).unwrap()),
        value => value,
      };
      obis.convert(&ObisCode::new(0, 0, 42, 0, 0, 255), convert_string);
      obis.convert(&ObisCode::new(0, 0, 96, 1, 0, 255), convert_string);

      Ok(obis)
    },
    err => err,
  });

  let mut thing = BaseThing::new(
    "urn:dev:ops:smart-meter-1".to_owned(),
    "Smart Meter".to_owned(),
    Some(vec!["MultiLevelSensor".to_owned()]),
    Some("A smart energy meter".to_owned()),
  );

  let first_response = smart_meter.next().unwrap().context("Failed to receive initial message from smart meter")?;

  for (obis_code, reg) in first_response.iter() {
    let level_description = json!({
        "@type": "LevelProperty",
        "title": obis_code,
        "type": "number",
        "unit": reg.unit(),
        "readOnly": true,
    });
    let level_description = level_description.as_object().unwrap().clone();
    thing.add_property(Box::new(BaseProperty::new(
      obis_code.to_string(),
      serde_json::to_value(reg.value()).unwrap(),
      None,
      Some(level_description),
    )));
  }

  let thing: Arc<RwLock<Box<dyn Thing>>> = Arc::new(RwLock::new(Box::new(thing)));
  let thing_clone = thing.clone();

  thread::spawn(move || {
    let thing = thing_clone;

    for res in smart_meter {
      let obis = res.unwrap();

      let mut thing = thing.write().unwrap();
      for (obis_code, reg) in obis.iter() {
        let property_name = obis_code.to_string();
        let new_value = serde_json::to_value(reg.value()).unwrap();
        let prop = thing.find_property(&property_name).unwrap();
        let _ = prop.set_cached_value(new_value.clone());
        thing.property_notify(property_name, new_value);
      }
    }
  });

  let mut server =
    WebThingServer::new(ThingsType::Single(thing), Some(port), None, None, Box::new(Generator), None, Some(true));
  server.start(None).await.context("Failed to start web server")
}
