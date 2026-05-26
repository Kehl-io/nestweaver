use std::collections::HashMap;
use crate::config::Settings;

pub struct SensorManager {
    sensors: HashMap<String, Sensor>,
}

pub enum SensorKind {
    Temperature,
    Humidity,
}

pub trait Readable {
    fn read(&self) -> f64;
    fn calibrate(&mut self);
}

impl Readable for SensorManager {
    fn read(&self) -> f64 {
        self.get_primary().value()
    }

    fn calibrate(&mut self) {
        self.sensors.values_mut().for_each(|s| s.reset());
    }
}

impl SensorManager {
    pub fn new() -> Self {
        SensorManager {
            sensors: HashMap::new(),
        }
    }

    fn get_primary(&self) -> &Sensor {
        self.sensors.get("primary").unwrap()
    }
}

pub fn initialize(config: &Settings) -> SensorManager {
    let mgr = SensorManager::new();
    println!("Initialized with {:?}", config);
    mgr
}
