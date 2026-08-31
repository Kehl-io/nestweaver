#pragma once
namespace demo {

class SensorRegistry {
public:
    void add(int id);
private:
    int count_;
};

struct SensorSlot {
    int pin;
    void reset() {
        pin = 0;
    }
};

} // namespace demo
