# Local patches on top of upstream

本檔記錄我們在 `local` 分支對上游 `nielsgroen/claude-tmux` 所做的改動，
用途是升級時對照（`git rebase <new-tag> local` 撞衝突時，看這裡理解「為什麼改」）。

## 升級流程

```
cd ~/src/claude-tmux
git fetch origin
git rebase <new-tag> local      # 例：git rebase v0.5.0 local
# 解衝突（參考下方每筆 patch 的意圖）
cargo build && cargo test
cargo install --path .
```

安裝後 `~/.cargo/.crates.toml` 會記成 path source，可一眼分辨這是本地版而非 registry 原版。

> 注意：tmux 那邊的「手機版顯示」客製在 `~/.tmux.conf`（F12 popup 90%×90%、mouse、
> Termius RGB、aggressive-resize），與本 fork 完全獨立，升級不影響，不需在這裡處理。

---

## Patch 列表

### P1 — 列表顯示 Claude 絕對閒置時長（基於 v0.4.0）

**動機**：要一眼看出每個 session「距離上一次 Claude 互動多久」，用來判斷 prompt cache
（Max 訂閱 1 小時 TTL）會不會過期、值不值得繼續該對話。需要**絕對時間**（跨 F12 重開仍準），
而不是「popup 開著期間」的相對計時。

**資料源選擇（實測 2026-05-28）**：
- ❌ tmux `session_activity`：閒置時會凍結沒錯，但被「打字回顯」「任意 pane 輸出」干擾，且分不清
  「閒置等輸入」與「正在跑長任務但沒輸出」。
- ❌ `~/.claude/sessions/<pid>.json` 的 `status` / `updatedAt`：實測不可靠（正在高頻互動的 session
  仍顯示 `status=busy` 且 `updatedAt` 卡在數分鐘前）。
- ✅ **transcript `.jsonl` 的 mtime**：每次 API 來回 / tool 事件都 append，閒置時凍結。
  `now - mtime` = 距上次互動秒數 = 正是 cache TTL 重置依據。免疫 TUI 重繪、絕對、跨 popup 重開準確。

**對應鏈**（因多個 session 可能共用 cwd，必須靠這條把 pane 對到正確的 transcript）:
```
pane tty (#{pane_tty})
  → 該 tty 上的 claude 進程 pid：掃 ~/.claude/sessions/<pid>.json，
     比對 readlink(/proc/<pid>/fd/0) == pane tty
  → 讀該 json 取 sessionId + cwd
  → ~/.claude/projects/<munge(cwd)>/<sessionId>.jsonl   （munge：'/' 與 '.' → '-'）
  → stat mtime → now - mtime = 閒置秒數
```

**改動檔案**：
- `src/claude_session.rs`（新檔）：上述對應鏈，`pub fn last_activity_for_tty(tty) -> Option<SystemTime>`。
  JSON 用輕量手解析（只取 sessionId/cwd 兩個無跳脫字串），不引入 serde_json 依賴。含 munge / 解析單元測試。
- `src/main.rs`：`mod claude_session;`。
- `src/session.rs`：`Pane` 新增 `tty: String`。
- `src/tmux.rs`：`list-panes` 格式加 `#{pane_tty}`，解析填入（`parts.len() >= 6`）。
- `src/app/mod.rs`：`App` 欄位 `last_active: HashMap<String, SystemTime>`（key = pane ID）；
  `tick_status()` 對每個 claude pane 解析 tty → mtime 存入；
  `idle_duration(pane_id)` = `now - last_active`。
- `src/ui/mod.rs`：free fn `format_idle`（`12s` / `3m` / `2h05m`）；row builder 在 Idle 時把標籤換成
  `idle {time}`，狀態欄寬 `{:<8}` → `{:<11}` 維持對齊。

**⚠️ 升級時必看（耦合 Claude Code 內部結構，改版可能壞）**：
- `~/.claude/sessions/<pid>.json` 的 schema（需要 `sessionId`、`cwd` 欄位）與「檔名 = pid」慣例。
- `~/.claude/projects/<munged-cwd>/<sessionId>.jsonl` 的路徑與 munge 規則（'/' 與 '.' → '-'）。
- `/proc/<pid>/fd/0` 指向 pane tty 的假設（Linux 限定；換 OS 要改 `pid_tty`）。
- 若上游日後自己加了 idle 時長或改了 `tick_status` / 狀態列渲染，rebase 時優先採用上游、再決定是否保留本 patch。
- 驗證方式：F12 列表的 `idle Xs` 應等於 `date +%s` 減該 session transcript jsonl 的 `stat -c %Y`。

### P2 — 把 idle > 1h 的 session 分到「需要 recache」區（依賴 P1）

**動機**：prompt cache TTL 1 小時。閒置超過 1h 的對話再開要付整段 recache，想在列表把這些
session 視覺上分到下方一區，一眼挑出「續用要付代價」的。

**做法**：閒置（P1 的 `idle_duration`）≥ 3600s 的 session 穩定排序到列表底部，並在它們上方插一條
分隔列「─ 需要 recache（idle > 1h）─」。門檻常數 `RECACHE_IDLE` 在 `App::needs_recache`（目前 3600s）。

**改動檔案**：
- `src/app/mod.rs`
  - `needs_recache(&self, session) -> bool`：`idle_duration >= RECACHE_IDLE`。idle 未知（解析失敗）視為 false（留在上方）。
  - `filtered_sessions()`：對結果做 `sort_by_key(|s| self.needs_recache(s))`（穩定排序，stale 沉底，組內維持原 attached/name 序）。
  - `recache_boundary() -> Option<usize>`：第一個 stale session 的索引（= fresh 數量）；無 stale 回 None（不畫分隔列）。
  - `compute_flat_list_index()` / `compute_total_list_items()`：有 stale 群時各 +1，補償分隔列佔的一行（否則捲動/高亮對不準）。
- `src/ui/mod.rs`：render 迴圈在 `i == recache_boundary` 時先 push 一條黃色全寬分隔列。

**注意 / 升級時留意**：
- 分隔列是「非可選取」的視覺列，只進 render 的 `items`，不進邏輯 session 清單；故 `compute_flat_list_index`/
  `compute_total_list_items` 的 +1 補償是正確性關鍵，改動列表渲染時要一起維護。
- session 跨越 1h 門檻時會跳到下方群組，因選取是「依索引」非「依身分」，極少數情況選取會落到相鄰 session（可接受，與上游既有 attached 變動時的行為一致）。
- 驗證方式：暫時把 `RECACHE_IDLE` 改小（如 60s）重編譯，閒置 session 應落到分隔列下方；驗完改回 3600。
