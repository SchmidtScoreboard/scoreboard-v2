// MatrixPortal S3 firmware: connects to WiFi + the scoreboard server over a
// websocket, runs the device side of the protocol (Hello -> Auth -> Manifest ->
// NeedFrames -> binary frames), blits RGB565 frames to a 64x32 HUB75 panel, and
// uses the seesaw rotary encoder to adjust brightness.
//
// First bring-up only: WiFi + server come from config.h. The app-driven BLE
// setup flow is a later build.

#include <Arduino.h>
#include <WiFi.h>
#include <WebSocketsClient.h>
#include <ArduinoJson.h>
#include <Adafruit_Protomatter.h>
#include <Adafruit_seesaw.h>
#include <map>
#include <vector>
#include "config.h"

// ---- panel (MatrixPortal S3 HUB75 pins) ----
static const int W = 64, H = 32, NPIX = W * H, FBYTES = NPIX * 2;
uint8_t rgbPins[]  = {42, 41, 40, 38, 39, 37};
uint8_t addrPins[] = {45, 36, 48, 35}; // A,B,C,D (64x32 = 1/16 scan; E/21 unused)
uint8_t clockPin = 2, latchPin = 47, oePin = 14;
Adafruit_Protomatter matrix(W, 5 /*bit depth*/, 1, rgbPins, 4, addrPins, clockPin, latchPin, oePin, true);

// ---- encoder ----
Adafruit_seesaw ss;
#define SS_SWITCH 24
bool haveEncoder = false;
int32_t lastEnc = 0;
uint8_t brightness = 200; // local, applied when blitting

// ---- websocket + display state ----
WebSocketsClient ws;
std::map<String, std::vector<uint8_t>> frameCache; // id -> RGB565 LE pixels
struct RotEntry { String id; uint32_t dwell; };
std::vector<RotEntry> rotation;
bool displayOn = true;
size_t rotIdx = 0;
uint32_t lastAdvance = 0;
bool needsBlit = false;

void blitCurrent() {
  if (!displayOn || rotation.empty()) { matrix.fillScreen(0); matrix.show(); return; }
  auto it = frameCache.find(rotation[rotIdx].id);
  if (it == frameCache.end()) return; // not fetched yet
  const std::vector<uint8_t>& px = it->second;
  if ((int)px.size() < FBYTES) return;
  for (int i = 0; i < NPIX; i++) {
    uint16_t c = (uint16_t)px[2 * i] | ((uint16_t)px[2 * i + 1] << 8); // LE RGB565
    uint16_t r = (c >> 11) & 0x1F, g = (c >> 5) & 0x3F, b = c & 0x1F;
    uint16_t r8 = (r << 3) | (r >> 2), g8 = (g << 2) | (g >> 4), b8 = (b << 3) | (b >> 2);
    r8 = r8 * brightness / 255; g8 = g8 * brightness / 255; b8 = b8 * brightness / 255;
    matrix.drawPixel(i % W, i / W, matrix.color565(r8, g8, b8));
  }
  matrix.show();
}

void onWsEvent(WStype_t type, uint8_t* payload, size_t len) {
  switch (type) {
    case WStype_CONNECTED:
      Serial.println("[ws] connected");
      break;
    case WStype_DISCONNECTED:
      Serial.println("[ws] disconnected");
      break;
    case WStype_TEXT: {
      JsonDocument doc;
      if (deserializeJson(doc, payload, len)) break;
      const char* t = doc["t"];
      if (!t) break;
      if (!strcmp(t, "Hello")) {
        JsonDocument a;
        a["t"] = "Auth"; a["role"] = "Device"; a["pubkey"] = BOARD_ID; a["sig"] = "";
        String out; serializeJson(a, out); ws.sendTXT(out);
        Serial.println("[ws] sent Auth");
      } else if (!strcmp(t, "Manifest")) {
        displayOn = doc["display_on"] | true;
        rotation.clear();
        std::vector<String> need;
        for (JsonObject e : doc["rotation"].as<JsonArray>()) {
          String id = e["frame_id"].as<String>();
          rotation.push_back({id, (uint32_t)(e["dwell_ms"] | 7000)});
          if (frameCache.find(id) == frameCache.end()) need.push_back(id);
        }
        // prune cache to only the ids in the new rotation
        for (auto it = frameCache.begin(); it != frameCache.end();) {
          bool keep = false;
          for (auto& r : rotation) if (r.id == it->first) { keep = true; break; }
          it = keep ? std::next(it) : frameCache.erase(it);
        }
        rotIdx = 0; lastAdvance = millis(); needsBlit = true;
        if (!need.empty()) {
          JsonDocument nd; nd["t"] = "NeedFrames";
          JsonArray ids = nd["ids"].to<JsonArray>();
          for (auto& id : need) ids.add(id);
          String out; serializeJson(nd, out); ws.sendTXT(out);
          Serial.printf("[ws] need %d frame(s)\n", (int)need.size());
        }
      }
      break;
    }
    case WStype_BIN: {
      if (len < 64) break; // [64 ASCII hex id][RGB565 pixels]
      String id((const char*)payload, 64);
      frameCache[id] = std::vector<uint8_t>(payload + 64, payload + len);
      needsBlit = true;
      Serial.printf("[ws] frame %.*s (%d bytes)\n", 8, (const char*)payload, (int)(len - 64));
      break;
    }
    default: break;
  }
}

void setup() {
  Serial.begin(115200);
  delay(200);

  ProtomatterStatus st = matrix.begin();
  Serial.printf("[matrix] protomatter status %d (0=ok)\n", st);
  matrix.fillScreen(0); matrix.show();

  if (ss.begin(0x36)) {
    ss.pinMode(SS_SWITCH, INPUT_PULLUP);
    lastEnc = ss.getEncoderPosition();
    haveEncoder = true;
    Serial.println("[seesaw] encoder ready");
  } else {
    Serial.println("[seesaw] not found (continuing without encoder)");
  }

  Serial.printf("[wifi] connecting to '%s'\n", WIFI_SSID);
  WiFi.mode(WIFI_STA);
  WiFi.begin(WIFI_SSID, WIFI_PASSWORD);
  // Non-blocking: loop() reports status and the WS client retries on its own.

  ws.begin(SERVER_HOST, SERVER_PORT, "/ws");
  ws.onEvent(onWsEvent);
  ws.setReconnectInterval(3000);
}

void loop() {
  ws.loop();

  static uint32_t lastStatus = 0;
  if (millis() - lastStatus > 5000) {
    lastStatus = millis();
    Serial.printf("[status] wifi=%d ip=%s ws=%d brightness=%d frames=%d\n",
                  (int)WiFi.status(), WiFi.localIP().toString().c_str(),
                  (int)ws.isConnected(), (int)brightness, (int)frameCache.size());
  }

  if (haveEncoder) {
    int32_t e = ss.getEncoderPosition();
    if (e != lastEnc) {
      int nb = (int)brightness + (e - lastEnc) * 16;
      brightness = (uint8_t)constrain(nb, 8, 255);
      lastEnc = e;
      needsBlit = true;
      Serial.printf("[enc] brightness %d\n", brightness);
    }
  }

  uint32_t now = millis();
  if (displayOn && rotation.size() > 1 && now - lastAdvance >= rotation[rotIdx].dwell) {
    rotIdx = (rotIdx + 1) % rotation.size();
    lastAdvance = now;
    needsBlit = true;
  }

  if (needsBlit) { blitCurrent(); needsBlit = false; }
  delay(15);
}
