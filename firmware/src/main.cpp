// GripApprover ファームウェア
// ハンドグリップを握り切った回数を、BLE と USB シリアルの両方で知らせる。承認の判断は PC 側が行う。
//
// BLE（デバイス名 GripApprover）
//   握力カウント: Read / Notify、uint32 リトルエンディアンの累計回数
//   コマンド:     Write、1バイト（A=承認の音 / D=拒否の音）
// USB シリアル
//   送る行:   grip N（握るたびに累計回数）/ count N（? への返事）
//   受ける文字: A=承認の音 / D=拒否の音 / ?=累計回数を返す
#include <Arduino.h>
#include <BLE2902.h>
#include <BLEDevice.h>
#include <BLEServer.h>
#include <BLEUtils.h>

#define SERVICE_UUID "8f1e0001-4b1a-4c3e-9d2a-6a7b6e0c1a01"
#define COUNT_UUID "8f1e0002-4b1a-4c3e-9d2a-6a7b6e0c1a01"
#define CMD_UUID "8f1e0003-4b1a-4c3e-9d2a-6a7b6e0c1a01"

const int GRIP_PIN = 4;     // アルミの接点。握り切ると GND と導通して LOW
const int BUZZER_PIN = 18;  // アクティブブザー
const int LED_PIN = 2;      // 基板上 LED

const unsigned long DEBOUNCE_MS = 30;

BLECharacteristic *countChar = nullptr;
volatile bool bleConnected = false;
volatile bool restartAdvertising = false;
volatile char pendingCmd = 0;  // BLE の onWrite で受けたコマンド。loop() で処理する

uint32_t gripCount = 0;
int stableState = HIGH;
int lastRead = HIGH;
unsigned long lastChangeMs = 0;
unsigned long ledOffAt = 0;

void beep(unsigned long ms) {
  digitalWrite(BUZZER_PIN, HIGH);
  delay(ms);
  digitalWrite(BUZZER_PIN, LOW);
}

void playApprove() {
  for (int i = 0; i < 3; i++) {
    beep(60);
    delay(60);
  }
  beep(400);
}

void playDeny() { beep(800); }

void handleCommand(char c) {
  if (c == '?') {
    Serial.printf("count %lu\n", (unsigned long)gripCount);
  } else if (c == 'A') {
    Serial.println("approve");
    playApprove();
  } else if (c == 'D') {
    Serial.println("deny");
    playDeny();
  }
}

class ServerCallbacks : public BLEServerCallbacks {
  void onConnect(BLEServer *) override { bleConnected = true; }
  void onDisconnect(BLEServer *) override {
    bleConnected = false;
    restartAdvertising = true;  // 切れたらすぐ見つけてもらえるように、loop() で広告を再開する
  }
};

class CmdCallbacks : public BLECharacteristicCallbacks {
  void onWrite(BLECharacteristic *c) override {
    std::string v = c->getValue();
    if (!v.empty()) pendingCmd = v[0];
  }
};

void setCountValue() {
  uint8_t buf[4] = {
      (uint8_t)(gripCount & 0xff),
      (uint8_t)((gripCount >> 8) & 0xff),
      (uint8_t)((gripCount >> 16) & 0xff),
      (uint8_t)((gripCount >> 24) & 0xff),
  };
  countChar->setValue(buf, sizeof(buf));
}

void setupBle() {
  BLEDevice::init("GripApprover");
  BLEServer *server = BLEDevice::createServer();
  server->setCallbacks(new ServerCallbacks());

  BLEService *service = server->createService(SERVICE_UUID);
  countChar = service->createCharacteristic(
      COUNT_UUID, BLECharacteristic::PROPERTY_READ | BLECharacteristic::PROPERTY_NOTIFY);
  countChar->addDescriptor(new BLE2902());
  setCountValue();

  BLECharacteristic *cmdChar = service->createCharacteristic(
      CMD_UUID, BLECharacteristic::PROPERTY_WRITE | BLECharacteristic::PROPERTY_WRITE_NR);
  cmdChar->setCallbacks(new CmdCallbacks());

  service->start();

  BLEAdvertising *adv = BLEDevice::getAdvertising();
  adv->addServiceUUID(SERVICE_UUID);  // Mac はこの UUID で探す
  adv->setScanResponse(true);         // 名前はスキャン応答に入る
  BLEDevice::startAdvertising();
}

void setup() {
  Serial.begin(115200);
  pinMode(GRIP_PIN, INPUT_PULLUP);
  pinMode(BUZZER_PIN, OUTPUT);
  pinMode(LED_PIN, OUTPUT);
  digitalWrite(BUZZER_PIN, LOW);
  digitalWrite(LED_PIN, LOW);
  setupBle();
  Serial.println("ready");
}

void loop() {
  unsigned long now = millis();

  // 握り検知：30ms 安定したら確定し、HIGH→LOW の確定で1回とする
  int r = digitalRead(GRIP_PIN);
  if (r != lastRead) {
    lastRead = r;
    lastChangeMs = now;
  }
  if (now - lastChangeMs >= DEBOUNCE_MS && r != stableState) {
    stableState = r;
    if (stableState == LOW) {
      gripCount++;
      Serial.printf("grip %lu\n", (unsigned long)gripCount);
      setCountValue();
      if (bleConnected) countChar->notify();
      digitalWrite(LED_PIN, HIGH);
      ledOffAt = now + 100;
      beep(15);
    }
  }

  if (ledOffAt && (long)(now - ledOffAt) >= 0) {
    digitalWrite(LED_PIN, LOW);
    ledOffAt = 0;
  }

  while (Serial.available()) handleCommand((char)Serial.read());

  char cmd = pendingCmd;
  if (cmd) {
    pendingCmd = 0;
    handleCommand(cmd);
  }

  if (restartAdvertising) {
    restartAdvertising = false;
    delay(100);
    BLEDevice::startAdvertising();
  }

  delay(1);
}
