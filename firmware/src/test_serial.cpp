// Minimal bring-up test (build with `pio run -e serialtest -t upload`).
// Isolates serial + panel from WiFi/websocket/seesaw: prints over USB and
// fills the panel solid red. If serial prints and the panel turns red, the
// USB-CDC + Protomatter basics work and the bug is in the networking code.
#include <Arduino.h>
#include <Adafruit_Protomatter.h>

uint8_t rgbPins[] = {42, 41, 40, 38, 39, 37};
uint8_t addrPins[] = {45, 36, 48, 35};
Adafruit_Protomatter matrix(64, 5, 1, rgbPins, 4, addrPins, 2, 47, 14, true);

void setup() {
  Serial.begin(115200);
  delay(800);
  Serial.println("[test] setup start");
  ProtomatterStatus st = matrix.begin();
  Serial.printf("[test] matrix.begin status = %d (0=OK)\n", (int)st);
  matrix.fillScreen(matrix.color565(80, 0, 0)); // dim red
  matrix.show();
  Serial.println("[test] filled red");
}

void loop() {
  static unsigned n = 0;
  Serial.printf("[test] alive %u\n", n++);
  delay(1000);
}
