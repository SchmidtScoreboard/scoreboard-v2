# MatrixPortal S3 firmware

First-bring-up firmware for the Adafruit MatrixPortal S3 + 64×32 HUB75 panel +
Adafruit I2C STEMMA QT rotary encoder. Connects to the scoreboard server over
WiFi and renders frames. (WiFi/server are configured in `src/config.h`; the
app-driven BLE setup flow is a later build.)

## Flash it

1. Install PlatformIO if needed: `brew install platformio` (or `pip3 install platformio`).
2. Create your local config (gitignored, so creds never get committed) and fill in WiFi:
   ```
   cp src/config.example.h src/config.h     # then edit WIFI_SSID / WIFI_PASSWORD
   ```
   `SERVER_HOST` is your laptop's LAN IP (currently `192.168.4.45`).
3. Connect the board's **USB-C to the laptop with a data cable** (not just panel power).
4. From this `firmware/` dir:
   ```
   pio run -t upload
   pio device monitor          # watch it connect (115200 baud)
   ```
   If upload can't find the port, double-tap the board's reset to enter the bootloader.

## Run the server (on the laptop)

```
cd ..  &&  cargo run -p server      # binds 0.0.0.0:7878
```

The board connects to `ws://192.168.4.45:7878/ws`, authenticates as device
`matrixportal-01`, and shows the live rotation. Rotate the encoder to change
brightness. The iOS app (subscribing to `matrixportal-01`) can then set favorite
teams to filter what this board shows.

## Notes / if the panel is blank

- Pins are set for the MatrixPortal S3 in `src/main.cpp` (`rgbPins`/`addrPins`/etc.).
  If the panel is blank or garbled, re-check them against Adafruit's MatrixPortal S3
  Protomatter example.
- This firmware is **untested on hardware** as written — expect to iterate.
