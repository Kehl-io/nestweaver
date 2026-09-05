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
    // nw-434: calls with an explicit template-argument list between the
    // callee name and the argument list. tree-sitter-cpp folds these into a
    // distinct `template_function`/`template_method` callee node instead of
    // the plain `identifier`/`qualified_identifier`/`field_expression` a
    // same-shaped call without `<...>` would use, so a query that only
    // handles the untemplated forms drops the call entirely.
    logGeneric<double>(temp);           // free function
    sensors::logGeneric<double>(temp);  // qualified (namespace- or class-scoped)
    mgr.logReading<int>(temp);          // member call (`obj.f<T>(...)`)
}
