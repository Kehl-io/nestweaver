#include <iostream>
#include "sensor.h"

class SensorManager {
public:
    void initialize();
    double readTemperature();
private:
    int sensorId;
};

void SensorManager::initialize() {
    calibrate();
}

double SensorManager::readTemperature() {
    return getReading(sensorId);
}

struct SensorConfig {
    int pin;
    double threshold;
};

enum SensorType {
    TEMPERATURE,
    HUMIDITY,
    PRESSURE
};

void setup() {
    SensorManager mgr;
    mgr.initialize();
    double temp = mgr.readTemperature();
    logValue(temp);
}
