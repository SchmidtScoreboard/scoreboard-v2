#pragma once
// Template. Copy to config.h (which is gitignored) and fill in your WiFi:
//     cp src/config.example.h src/config.h
#define WIFI_SSID     "YOUR_WIFI_SSID"
#define WIFI_PASSWORD "YOUR_WIFI_PASSWORD"

// Your laptop's LAN IP running `cargo run -p server`.
#define SERVER_HOST   "192.168.4.45"
#define SERVER_PORT   7878

// This board's identity (the app subscribes to this id to control it).
#define BOARD_ID      "matrixportal-01"
