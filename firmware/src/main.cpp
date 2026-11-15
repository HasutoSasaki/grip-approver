// GripApprover ファームウェア
// ハンドグリップを握り切った回数を USB シリアルで知らせる。承認の判断は PC 側が行う。
//
// 送る行:   grip N（握るたびに累計回数）/ count N（? への返事）
// 受ける文字: A=承認の音 / D=拒否の音 / ?=累計回数を返す
#include <Arduino.h>

const int GRIP_PIN = 4;     // アルミテープ接点。握り切ると GND と導通して LOW
const int BUZZER_PIN = 18;  // アクティブブザー。会議ランプ（GPIO25・26）と反対側の列のピン
const int LED_PIN = 2;      // 基板上 LED

const unsigned long DEBOUNCE_MS = 30;

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

void setup() {
  Serial.begin(115200);
  pinMode(GRIP_PIN, INPUT_PULLUP);
  pinMode(BUZZER_PIN, OUTPUT);
  pinMode(LED_PIN, OUTPUT);
  digitalWrite(BUZZER_PIN, LOW);
  digitalWrite(LED_PIN, LOW);
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
      digitalWrite(LED_PIN, HIGH);
      ledOffAt = now + 100;
      beep(15);
    }
  }

  if (ledOffAt && (long)(now - ledOffAt) >= 0) {
    digitalWrite(LED_PIN, LOW);
    ledOffAt = 0;
  }

  while (Serial.available()) {
    int c = Serial.read();
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

  delay(1);
}
