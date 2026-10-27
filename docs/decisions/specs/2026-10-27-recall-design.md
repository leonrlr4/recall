# recall 設計

> Historical record, frozen 2026-10-27. Source code is authoritative; where
> this document and the code disagree, the code wins.

## 目的

recall 記錄這台電腦上開的每一場線上會議：自動偵測開始與結束，錄下麥克風和系統輸出兩條音軌，可選錄影，會後在本機轉成繁體中文與英文混排的逐字稿，分辨講者並記住每個人的聲紋，用本地或雲端 LLM 產出摘要、決策、待辦，放進一個可搜尋的網頁檔案庫。音訊和文字都留在本機，除非使用者明確選擇 Claude API 當摘要後端。

使用者是 Leon，會議多在 Google Meet（Brave）、Slack huddle、Discord，語言是台灣國語與英文混雜。效能目標是 Rust 單一 binary，不依賴 Python。

### 成功標準

- 開會不用按任何鍵，會後 10 分鐘內（60 分鐘會議）網頁上有逐字稿和摘要。
- 中英混雜句子的錯誤率低於 Whisper large-v3 一半（以 bench 的 ASCEND 類案例量）。
- 「我」和「別人」100% 分對；別人之間的講者在第二次開會後自動對上名字。
- daemon 閒置時 CPU 0%，錄音時單核 2% 以內。
- 任何一步失敗都保留音檔，能從該步重跑。

### 不做

- 行事曆連動（討論過，暫緩）。
- 雲端同步、多使用者。
- Tauri 外殼（網頁先行，之後可薄薄包一層指向本機網址，不改 daemon 和前端）。

## 既有專案的位置

調查了 Meetily、Screenpipe、Anarlog、Vibe、Minutes、omarchy-meeting-recorder、mnemosyne 等開源專案，和 Granola、Otter、Fathom、飛書妙記等商業產品。沒有一個做中英混雜 STT 或繁體輸出，沒有一個在 Linux 上真的能從 PipeWire 偵測會議，所有 Linux 選項都是「一場會議的工作台」而非「所有會議的檔案庫」。

omarchy-meeting-recorder（GTK4，Rust）是最接近的，錄音與轉錄品質認真，值得照抄：錄前即時波形、回音規則、崩潰復原、可編輯逐字稿、bench 套件。它缺列表、搜尋、摘要、自動偵測、中文。決定從頭做，把它和 Anarlog 的 `crates/audio-sync` 當參考碼。

## 存放格式

每場會議一個目錄，預設在 `~/Meetings/`，目錄名是開始時間加標題：

```
~/Meetings/2026-10-27 14:30 Biobuild weekly/
├── meeting.md      摘要與逐字稿，Obsidian 直接讀
├── audio.opus      立體聲 48 kHz，左聲道麥克風（我），右聲道系統輸出（別人）
├── screen.mp4      有開錄影才有
├── meeting.json    機器用：時間、來源 app、講者 id、段落時間戳、各步狀態、模型版本
└── .cache/         錄製中的 20 秒分段暫存，完成後清掉
```

`meeting.md` 開頭是 YAML frontmatter（title、date、duration、participants、source_app、tags），接著 `## 摘要`、`## 決策`、`## 待辦`（每條帶負責人與期限）、`## 章節`、`## 逐字稿`。逐字稿每行 `[01:32] **Tom** 這段 export 會 timeout`。中文一律繁體，英文詞保留原樣。

講者是全域實體：

```
~/.local/share/recall/
├── speakers.json   id → 名字、別名、多個聲紋 embedding
├── index.sqlite    FTS5 全文索引與列表查詢；可隨時從目錄重建
└── models/         Breeze ggml、sherpa 模型
```

`meeting.json` 只存講者 id。改名在 `speakers.json` 改一次，daemon 重寫所有相關 `meeting.md`。SQLite 只是索引，真相永遠是目錄裡的檔案，rsync、git、Obsidian 都能直接用。

設定檔 `~/.config/recall/config.toml`：會議目錄、錄影開關、偵測白名單、debounce 秒數、LLM 後端（`ollama` 或 `claude`）與模型名、摘要語言、UI 語言、STT 引擎。

## daemon 狀態機與偵測

`recall daemon` 是 systemd user service，一個 process 四條線：PipeWire 監聽、錄製、處理佇列、HTTP。

```
Idle ──白名單 app 開麥克風──▶ Armed ──撐過 8 秒──▶ Recording
Idle ──快速鍵 start────────────────────────────────▶ Recording
Armed ──8 秒內關掉（權限探測）──▶ Idle
Recording ──麥克風關閉 30 秒或快速鍵 stop──▶ Finishing ──寫完 audio.opus──▶ Processing ──▶ Idle
Recording ──快速鍵 discard──▶ Idle（刪暫存）
```

偵測規則：

- 觸發：PipeWire registry 出現 `Stream/Input/Audio` 節點，且 `application.process.binary` 在白名單（預設 brave、chromium、firefox、zoom、slack、discord、teams）。忽略 recall 自己的節點。
- Armed 期間已在暫存錄音，正式開始時前 8 秒不會少。
- 結束要節點消失超過 30 秒，避免對方斷線重連被切成兩場。
- 短於 2 分鐘的錄音自動丟掉。
- 同時只錄一場，第二個 app 開麥克風併入同一場。
- PulseAudio-only 系統走 `libpulse-binding` 的 source_output 訂閱，同一套規則。

快速鍵 `recall start` / `stop` / `discard` 透過 `$XDG_RUNTIME_DIR/recall/control.sock` 送指令，任何桌面都能綁。

錄製中：

- 兩條 `pipewire` crate 的 stream：麥克風抓預設 source，系統音對預設 sink 設 `stream.capture.sink = true`。要求 F32 48 kHz，由 PipeWire 重取樣。
- 跟著 `default.audio.sink` metadata 切換，換耳機不斷；某軌短暫沒資料就補靜音，兩軌對齊。
- 每 20 秒一段寫進 `.cache/`，崩潰只掉最後 20 秒；下次啟動發現未完成錄音就接續處理。
- 錄影開啟時 spawn `gpu-screen-recorder -w portal`，用 restore token 免對話框；token 失效就發通知請點一次，音訊照錄。
- 桌面通知：開始「錄製中：Google Meet」、結束「處理中」、完成「摘要好了」附網頁連結。狀態同時寫 `$XDG_RUNTIME_DIR/recall/state.json` 給 bar 用。

處理佇列存在 SQLite，一次跑一個，daemon 重啟接著跑。

## 處理管線

六步依序跑，每步輸出寫進 `meeting.json`，失敗停在那步，網頁與 CLI 能從該步重跑。

1. **切段**：兩聲道各跑 Silero VAD（sherpa-onnx），只送有人聲區段。回音規則：左聲道只在右聲道有聲時才有聲的區段當回音丟掉。
2. **轉錄**：`whisper-rs`（Vulkan）跑 Breeze-ASR-25 ggml q8_0，兩聲道分開轉，語言提示 `zh`，再過 OpenCC `s2tw`。詞級時間戳用 DTW。引擎是 trait，第二個實作是 sherpa-onnx 跑 TEA-ASR-1.1，等 spike 驗證匯出與速度後再決定預設。
3. **分講者**：只跑右聲道，sherpa-onnx 的 pyannote segmentation-3.0 + CAM++ embedding。左聲道固定是「我」。輸出 Remote N 和每個 cluster 的 embedding。
4. **對名字**，依序填：聲紋比對 `speakers.json`（cosine 門檻先 0.6，用實際錄音校準，每人存多個 embedding，只在高信心時新增）；Meet 瀏覽器擴充送來的發言者事件做多數決（第一版不做擴充，格式先留欄位）；都沒對到留 Remote N，使用者在網頁改名後 embedding 存進此人。
5. **摘要**：帶講者的逐字稿送 LLM，固定輸出 JSON：一句話摘要、摘要段、決策、待辦（事項、負責人、期限）、章節（開始時間、標題）。Prompt 要求繁體、英文術語與人名原樣。超過上下文按章節分批再合併。後端 `ollama`（預設 `qwen3.6:35b-a3b`）或 `claude`（API key 從環境變數）。
6. **寫檔**：產生 `meeting.md`，更新索引，發通知。

效能目標（60 分鐘會議，Intel Arc B390）：轉錄 5 分鐘內、diarization 2 分鐘內、本地摘要 2 分鐘內。

Bench：`bench/fixtures/` 每案例音檔加 `truth.json`，`thresholds.json` 定門檻，CI 跑。案例抄 omarchy-meeting-recorder 的十個，加純國語、中英混雜、台灣口音英文三個。

## 網頁與 API

`axum` 在 `127.0.0.1:7474`，靜態檔 `rust-embed` 編進 binary。前端 Svelte 5 + Vite，`web/dist/` 進 repo，`cargo build` 不需要 Node。

介面字串在 `web/src/i18n/{zh-TW,en}.json`，跟瀏覽器語系，設定頁可鎖定。摘要語言是另一個設定。

頁面：

1. `/` 會議列表：搜尋框、篩選（來源 app、參與者、日期）；依日期分組的卡片，顯示標題、時長、參與者縮寫、一句摘要、處理狀態。
2. `/m/:id` 會議頁三欄：左摘要、決策、待辦、章節；中逐字稿，講者有色，點句播放，hover 改字、換講者、刪除；右上播放器（波形上下分我與別人，章節是刻度，有錄影時換影片），右下「問這場會議」。
3. `/people` 講者：名字、別名、出現過的會議、聲紋樣本數；改名即時套用；可合併。
4. `/settings` 設定表單，加「測試錄音」：錄 5 秒顯示兩聲道波形。

API（JSON，只綁本機）：

| 路徑 | 用途 |
|---|---|
| `GET /api/meetings?q=&app=&from=` | 列表與全文搜尋 |
| `GET/PATCH /api/meetings/:id` | 讀取、改標題、改段落 |
| `POST /api/meetings/:id/rerun/:step` | 從某步重跑 |
| `GET /api/meetings/:id/audio` | Range 串流 |
| `POST /api/meetings/:id/ask` | 問答，SSE |
| `GET/PATCH /api/people/:id` | 講者改名、合併 |
| `GET /api/events` | SSE：錄製狀態、處理進度 |

CLI 用同一組 API：`recall list`、`recall show <id>`、`recall rerun <id> summary`。

搜尋用 FTS5 `trigram` tokenizer，中文免分詞；結果附命中句和時間戳，點了跳播。

鍵盤：列表 `/` 搜尋、`j k` 上下、`Enter` 開；會議頁 `空白` 播放暫停、`[ ]` 前後句、`e` 改當前句、`s` 換講者。

## 專案結構

Cargo workspace：

```
recall/
├── crates/
│   ├── recall-audio      PipeWire 偵測、雙軌擷取、Opus 寫檔
│   ├── recall-pipeline   VAD、轉錄、diarization、對名字、摘要
│   ├── recall-store      會議目錄讀寫、speakers.json、SQLite 索引
│   ├── recall-llm        Ollama / Claude，同一個 trait
│   ├── recall-server     axum API、SSE、靜態檔
│   └── recall            binary：daemon、CLI、systemd 整合
├── web/                  Svelte，dist/ 進 repo
├── bench/
└── docs/decisions/
```

sherpa-onnx 和 whisper-rs 各自帶 runtime（ONNX Runtime、ggml），不衝突。不同時引入第二個帶 ONNX Runtime 的 crate（例如 pyannote-rs），Minutes 專案為此得把 sherpa 拆成 dlopen 的 cdylib。

## 測試

- 單元：store 格式往返、pipeline 純函數（回音規則、時間軸合併、名字投票）、LLM JSON 解析、i18n 檔案鍵一致。
- 整合：bench 音檔跑整條管線，不碰 PipeWire。
- 錄音：`recall selftest` 播音到 sink 同時錄 monitor；CI 用 PipeWire null sink。
- 偵測：`pw-cli` 建假 capture 節點測狀態機。

## 階段

| 階段 | 交付 |
|---|---|
| 1 | daemon 偵測、雙軌錄音、快速鍵、通知，產出 audio.opus |
| 2 | 轉錄、我/別人分軌、meeting.md、列表與會議頁、zh-TW/en UI |
| 3 | diarization、聲紋、講者頁、摘要與待辦、問答 |
| 4 | 錄影、Meet 擴充、MCP server、TEA-ASR spike |
| 5 | 即時字幕（sherpa-onnx 串流 X-ASR，會後再用階段 2 引擎重跑） |

每階段結束都能用；階段 1 後開始累積音檔，後面換模型可全部重跑。

## 已知風險

- Diarization 在中文多人重疊的 DER 約 12 到 20%，相似聲音會被合併；靠網頁的換講者與合併修。
- 瀏覽器 WebRTC 權限探測會開關麥克風，靠 8 秒 debounce；Brave 的其他麥克風用途（語音輸入）會被誤錄，靠 2 分鐘下限與 discard 鍵。
- xdph 的 restore token 可能回空，錄影第一次要點一次。
- `pipewire` crate 0.10 pre-1.0，API 會變。
- Breeze 用 large-v2 解碼器，比 turbo 慢；B390 上實測後若超過目標，改 TEA-ASR 或 SenseVoice。
- 錄下別人的聲音與聲紋涉及同意，使用者自行負責。
