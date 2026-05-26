#include "sensor.h"
#include <stdio.h>

typedef struct {
    int id;
    char name[64];
} SensorConfig;

enum SensorType {
    TEMPERATURE,
    HUMIDITY,
    PRESSURE
};

struct SensorManager {
    int count;
    SensorConfig configs[10];
};

void initialize(struct SensorManager* mgr) {
    mgr->count = 0;
}

static int calibrate(int sensor_id) {
    return sensor_id * 2;
}

int main(int argc, char** argv) {
    struct SensorManager mgr;
    initialize(&mgr);
    int val = calibrate(1);
    printf("Calibrated: %d\n", val);
    return 0;
}
