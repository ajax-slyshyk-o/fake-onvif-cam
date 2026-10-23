use crate::config::Config;
use crate::util;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Writes the cameras as JSON to `target`, or to stdout when `target` is `-`.
pub fn write(target: &Path, configs: &[Config]) -> io::Result<()> {
    let json = render(configs);

    if target == Path::new("-") {
        io::stdout().write_all(json.as_bytes())
    } else {
        fs::write(target, json)
    }
}

fn render(configs: &[Config]) -> String {
    let cameras: Vec<String> = configs.iter().map(camera_json).collect();
    format!("{{\n  \"cameras\": [\n{}\n  ]\n}}\n", cameras.join(",\n"))
}

fn camera_json(config: &Config) -> String {
    format!(
        "    {{\n      \"name\": \"{}\",\n      \"uuid\": \"{}\",\n      \"advertise_host\": \"{}\",\n      \"http_port\": {}\n    }}",
        util::json_escape(&config.camera_name),
        util::json_escape(&config.uuid),
        util::json_escape(&config.advertise_host),
        config.http_addr.port()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn camera(name: &str, uuid: &str, port: u16) -> Config {
        Config {
            camera_name: name.to_string(),
            uuid: uuid.to_string(),
            advertise_host: "10.0.0.5".to_string(),
            http_addr: format!("0.0.0.0:{port}").parse().unwrap(),
            ..Config::default()
        }
    }

    #[test]
    fn renders_all_cameras() {
        let configs = [
            camera("Fake \"01\"", "00000000-0000-4550-98a8-98a80cab1b88", 8000),
            camera("Fake 02", "00000000-0000-4550-98a8-98a80cab1b89", 8001),
        ];

        let expected = r#"{
  "cameras": [
    {
      "name": "Fake \"01\"",
      "uuid": "00000000-0000-4550-98a8-98a80cab1b88",
      "advertise_host": "10.0.0.5",
      "http_port": 8000
    },
    {
      "name": "Fake 02",
      "uuid": "00000000-0000-4550-98a8-98a80cab1b89",
      "advertise_host": "10.0.0.5",
      "http_port": 8001
    }
  ]
}
"#;
        assert_eq!(render(&configs), expected);
    }
}
