# CheckPo Windows 検証計画

作成日: 2026-07-15
対象: Windows 11 / Windows Server、NTFS、CheckPo Core / CLI / GUI

## 結論と合格条件

Windows 正式サポートの判定は、次のすべてを満たした場合だけ **Go** とする。

- release candidate と同じ単一 commit・単一 CLI SHA-256 で全試験を実行する
- `FlushFileBuffers` 失敗の no-op 継続や、安全性を落とす path-based fallback を入れない
- fresh repository から開始し、試験中にバイナリやスクリプトを変更しない
- registry 復元、stuck transaction 削除、journal 手動削除などの手動介入を一度も行わない
- 正常終了、process kill、OS crash / VM hard reset 後の全ケースで、データ消失・誤った成功応答・参照切れがない
- `status`、quick/full verify、index rebuild、GC の error / warning / missing / invalid が 0
- GUI の主要操作を実画面で確認する
- 暫定性能ゲートを満たすか、未達項目について正式リリース前の対応方針が決まっている

一度でもコードやバイナリを変更した場合、その時点までの結果は診断資料として保存し、合格系列は新しい空の benchmark root から取り直す。

## 2026-07-15 実施状況（履歴）

- 通常終了系のWindows / NTFSレビュー・自動テスト・Milfy規模ベンチ: **Go**
- 1000連続checkpoint、20,000 small files、2 GiB file、最終1004 status/diff/index/GC/full verify: 完了
- 最終multi-lens review: 残存P0/P1なし
- Tauri GUI実画面: 自動復元、1005 checkpoint履歴、正確な差分0、checkpoint verify、設定/保守分析、theme、通常終了、処理中キャンセル終了を確認
- GUIで発見したライトthemeのdisabled primary表示を修正し、当時のfrontend test、release build、実画面再確認を完了
- 詳細: `review-artifacts/windows-real-usage-benchmark-20260715.md`
- 未完了: 物理Windows 11 hard-power-off反復、Windows Server VM hard reset

したがって、この文書全体が定義する「Windows正式サポート」の最終Goではなく、通常終了系release candidateのGoである。この記録は当時のcommit・CLI・benchmark scriptにのみ対応する。現行candidateでは、同一commit・CLI・scriptのSHAを固定し、新しいbenchmark rootで再実施するまで合格根拠として使わない。

## 今回優先して検証する仮説

### P0: held-handle rename の native API 契約

現在の実装は `NtSetInformationFile` に `FILE_RENAME_INFORMATION` / `FILE_RENAME_INFORMATION_EX` を渡し、destination parent handleを`RootDirectory`として相対renameします。

検証ではinformation class、replaceフラグ、NTSTATUS、Win32 errorへの変換、source/destinationのidentityを記録し、junction差替えや同名差替え時に外部への書込み・誤renameが起きないことを確認する。成功したAPI呼出しだけではdurabilityを保証しない。

参考:

- [FILE_RENAME_INFORMATION (kernel / driver)](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/ns-ntifs-_file_rename_information)

### P0: directory durability barrier の前提不成立

現在の設計は directory handle に対する `FlushFileBuffers` を Unix の directory `fsync` 相当として扱う。Windows の公開仕様は、`FlushFileBuffers` に GENERIC_WRITE を持つ file handle を要求するが、directory namespace の永続化契約を明示していない。

次を切り分ける。

- directory を GENERIC_WRITE 付きで開く段階が `ACCESS_DENIED (5)` になるのか
- open は成功し、`FlushFileBuffers` が失敗するのか
- 管理者権限だけで成功するのか
- Windows 11 / Server、物理 NTFS / VM NTFS で結果が異なるのか
- 成功した場合も、hard reset 後の rename 永続化を本当に保証できるのか

参考:

- [FlushFileBuffers](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)

### P0: directory handle の `CreateFileW` 契約

現在のdirectory handle取得は`CreateFileW`を使う。requested access、share mode、`FILE_FLAG_BACKUP_SEMANTICS`、ACL、他プロセスが保持するhandleとの競合を実測し、一般ユーザーでdirectory barrierに必要な操作が成立することを確認する。

参考:

- [CreateFileW](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew)

### P0: registry 消失と journal cleanup mismatch

次のどれかをログと fault injection で切り分ける。

- registry の temporary write / file flush / replace / parent barrier の途中失敗
- atomic replace 後、directory entry が永続化される前の crash
- benchmark の CheckPoData コピー・復元操作による上書き
- 異なる CLI バイナリや複数プロセスによる同時更新
- Windows の delete-pending、共有モード、外部プロセスの handle 保持
- CheckPo 自身が保持した handle による cleanup 遅延

## 1. 検証環境

### 1.1 最低限の環境

最初は次の2環境を用意する。

1. Windows 11 最新安定版、物理 PC、内蔵 SSD、NTFS、標準ユーザー、Defender 有効
2. Windows Server VM、NTFS、標準ユーザー、Defender 有効

管理者実行は通常試験に使わない。管理者でのみ成功する操作は、一般ユーザー向け製品として不合格とする。管理者実行は原因切り分け用の比較ケースに限定する。

余裕があれば次も追加する。

- Windows 11 の別ハードウェア
- Windows Server の別 hypervisor
- 外付け NTFS SSD
- Defender 無効または対象フォルダ除外。ただし性能原因の切り分け専用とし、正式性能値は Defender 有効で取る
- exFAT、ReFS、SMB、OneDrive 配下。非対応とする場合は安全に拒否できることだけ確認する

### 1.2 記録する環境情報

各 run の開始前に PowerShell で取得し、`results/environment/` に保存する。

```powershell
New-Item -ItemType Directory -Force results\environment | Out-Null
Get-ComputerInfo | Out-File -Encoding utf8 results\environment\computer-info.txt
Get-CimInstance Win32_Processor | Format-List * | Out-File -Encoding utf8 results\environment\cpu.txt
Get-CimInstance Win32_PhysicalMemory | Format-List * | Out-File -Encoding utf8 results\environment\memory.txt
Get-CimInstance Win32_DiskDrive | Format-List * | Out-File -Encoding utf8 results\environment\disk.txt
Get-Volume | Format-List * | Out-File -Encoding utf8 results\environment\volumes.txt
fsutil fsinfo volumeinfo C: | Out-File -Encoding utf8 results\environment\fsutil-volume.txt
Get-MpComputerStatus | Format-List * | Out-File -Encoding utf8 results\environment\defender.txt
powercfg /getactivescheme | Out-File -Encoding utf8 results\environment\power-scheme.txt
git rev-parse HEAD | Out-File -Encoding ascii results\environment\git-head.txt
git status --porcelain=v1 | Out-File -Encoding utf8 results\environment\git-status.txt
rustc -Vv | Out-File -Encoding utf8 results\environment\rustc.txt
cargo -V | Out-File -Encoding utf8 results\environment\cargo.txt
python --version 2>&1 | Out-File -Encoding utf8 results\environment\python.txt
```

VM では、上記に加えて次を手動で記録する。

- hypervisor 名とバージョン
- vCPU 数、割り当て RAM
- 仮想ディスク形式、固定 / 可変、write cache 設定
- 仮想ディスクが置かれた host 側ストレージ
- checkpoint / snapshot の有無
- hard reset の実行方法

### 1.3 成果物の固定

```powershell
Get-FileHash .\target\release\checkpo.exe -Algorithm SHA256
Get-FileHash .\scripts\checkpo_real_usage_benchmark.py -Algorithm SHA256
git rev-parse HEAD
git status --porcelain=v1
```

必須条件:

- CLI と benchmark script の SHA-256 を `environment.json` に保存する
- run 再開時に SHA が一致しなければ fail-closed で停止する
- dirty tree の場合は diff を成果物に保存する。正式合格 run は原則 clean commit で行う
- debug build と release build の結果を混ぜない
- retry は同じ repository へ重ねず、新しい benchmark root を使う

## 2. Windows benchmark driver の準備

既存の `scripts/checkpo_real_usage_benchmark.py` はWindowsを分岐でサポートしている。Windowsでは`GetVolumeInformationW`でfilesystem情報を取得し、source projectのcopyには`robocopy`、wall timeには`time.perf_counter_ns()`を使う。

ただし`maximumResidentSetBytes`はWindowsで現在`null`である。peak memoryを取得できる値として報告してはならず、必要になった時点でWindows API、PowerShell、またはpsutilによる実測を追加してから性能ゲートに含める。

正式runでは次を確認する。

- source project のコピー方法と所要時間を記録する
- Windows filesystem / disk / Defender 情報を保存する
- CLI / script SHA の固定を維持する
- `100 checkpoints -> ops-100 -> 500 -> ops-500 -> 1000 -> ops-1000` の順で interleave する
- small-files 後と large-file 後に clean checkpoint を自動作成する
- scenario retry を同じ state directory へ混ぜない
- JSONL は各行 flush し、成功した操作と記録の欠落を検知する
- raw stdout / stderr、exit code、raw Win32 error、Core timings を保存する

driver の移植確認だけを目的にした小規模 run と、正式性能 run を分ける。

## 3. P0 API 診断試験

この試験では最初から fallback を入れない。失敗する API、引数、raw Win32 error を確定する。

全呼び出しについて次をログに残す。

- API 名
- source / destination の種別と同一 volume か
- desired access mask
- share mode
- flags / information class
- file / directory / reparse point
- 同時に開いている handle 数と用途
- 戻り値と `GetLastError()` の数値・名称
- rename 前後の volume serial、FileId、path

### 3.1 rename matrix

以下を file と directory の両方で実施する。

| ケース | replace | parent | held source | held destination | 期待 |
|---|---:|---|---:|---:|---|
| 同一 directory 内 rename | no | same | yes | yes | 成功、FileId 維持 |
| 同一 directory 内 replace | yes | same | yes | yes | 成功、destination が source FileId |
| 別 directory へ rename | no | different | yes | yes | 成功または明示的な非対応 |
| 別 directory へ replace | yes | different | yes | yes | 成功または明示的な非対応 |
| destination 存在 | no | same / different | yes | yes | AlreadyExists、既存内容不変 |
| source readonly | no / yes | same / different | yes | yes | 仕様どおり、安全に失敗 |
| destination readonly | yes | same / different | yes | yes | 仕様どおり、安全に失敗 |
| source を他 process が保持 | no / yes | same / different | yes | yes | timeout / error、データ不変 |
| destination を他 process が保持 | yes | same / different | yes | yes | timeout / error、データ不変 |
| parent を junction に差替え | no / yes | same / different | yes | yes | 外部へ書かない |
| source 名を同名別 file に差替え | no / yes | same / different | yes | yes | identity mismatch で失敗 |

現在の`NtSetInformationFile`を対象に、`FILE_RENAME_INFORMATION`と`FILE_RENAME_INFORMATION_EX`のinformation class・replaceフラグを分けてprobeする。probeの成功だけで安全と判断せず、path swap / junction、OS version、クラッシュ後durabilityを含めて判断する。

### 3.2 directory flush matrix

次の access / flags で directory open と `FlushFileBuffers` の結果を記録する。

- GENERIC_READ
- GENERIC_WRITE
- GENERIC_READ | GENERIC_WRITE
- FILE_READ_ATTRIBUTES | SYNCHRONIZE
- FILE_FLAG_BACKUP_SEMANTICS
- FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH
- standard user / administrator
- Windows 11 physical NTFS / Server VM NTFS

成功した組合せは、rename 後すぐに hard reset する反復試験へ進める。API が成功したという事実だけで durable barrier と判定しない。

### 3.3 `CreateFileW` directory-handle matrix

- directory handle の desired access / share mode / flags を全組合せで記録する
- 必要最小限のaccessでidentity readback、relative rename、directory barrierが可能か確認する
- CheckPo 自身が保持する handle との share conflict を確認する
- Defender / indexer が開いている場合との差を ProcMon で確認する

## 4. ビルド・自動テスト

release candidate を作る前に、Windows 上で次を実行する。

```powershell
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace --release
node --test src/CheckPo.Tauri/frontend/frontend-state.test.js
```

確認事項:

- Windows 専用テストが実際に列挙・実行されている
- frontend testはWindowsで実行していることを確認する
- ignored scale test は別途明示実行する
- debug / release の双方で correctness test を通す
- test process の残留、temp file、journal、lock file がない
- panic、access violation、handle leak がない

## 5. 正常終了 E2E

### 5.1 最小 smoke

空の benchmark root と小さな Unity project で次を確認する。

1. `init`
2. checkpoint create
3. list / status
4. file add / modify / delete の diff
5. restore preview / apply
6. discard preview / apply
7. checkpoint rename / delete
8. quick verify / full verify
9. index rebuild
10. GC analyze / apply / analyze
11. CLI process を終了して再起動後、同じ結果になること

各 destructive apply は preview の plan を保存し、apply 時の expected plan と完全一致させる。

### 5.2 実利用 scale

30,000 files / 約7.65 GiB の同一 Unity project を使う。

1. fresh 初回 checkpoint
2. checkpoint 2..100
3. ops-100
4. checkpoint 101..500
5. ops-500
6. checkpoint 501..1000
7. ops-1000 + full verify
8. 20,000 small-files scenario
9. clean checkpoint
10. 2 GiB large-file scenario
11. clean checkpoint
12. final status / diff / index rebuild / GC / full verify

各 milestone で確認する。

- checkpoint number が連続
- series number と ID が全件 unique、null ID 0
- latest name / ID が series と一致
- diff latest が added / modified / deleted 0
- index が current、snapshot count が一致
- pending transaction / unresolved quarantine / warning が 0
- GC 前後で missing / invalid / unreferenced が 0
- quick verify が valid、1000件では full verify も valid

## 6. 正しさ・破損検出試験

以下は Windows 上で必須。

- 同一 size・同一 mtime の内容変更を checkpoint / full diff が検出する
- file を同名別 FileId に差し替えた場合、fingerprint cache が変更を隠さない
- mtime だけ変更した場合、metadata-only change として扱う
- object 1件を同一 size で改ざんし、full verify が検出する
- object を削除し、verify / GC / index rebuild が missing を報告する
- object cache が破損していても次 checkpoint が誤った object を再利用しない
- registry JSON の truncate / invalid JSON / missing を安全に報告し、新規 repository を上書き作成しない
- journal / inventory / refs/latest の各1件を破損し、誤った成功扱いをしない
- preview 後、apply 前に working tree を変更し、expected-plan mismatch で失敗する
- scan / hash 中に対象 file を書換え、古い hash と新しい metadata を混ぜない

fast-path は「size + mtime だけ」で合格にしない。Windows では少なくとも volume serial、FileId、size、creation / last-write / change time、attributes を含む fingerprint と、baseline の object ID を照合する。fingerprint が取得不能または不一致なら hash へ戻す。

## 7. crash / power-loss recovery

### 7.1 fault injection の原則

時間指定の強制終了だけでは狙った境界を再現できない。test-only fault point を設け、対象 phase 到達を外部へ通知してから停止できるようにする。

fault point は本番 release で有効化できない構成にする。各 point について最低20回、重要な commit point は100回反復する。

停止方法を分ける。

- process kill: `Stop-Process -Force` または `TerminateProcess`
- OS crash: test VM の crash / forced reboot
- VM hard reset: guest shutdownを待たず host から power off / reset
- 物理 power-loss: 最終 release gate として別途実機で実施

### 7.2 checkpoint fault points

- create journal temporary write 後
- create journal file flush 後
- create journal publish 後
- object temporary write 中
- object file flush 後
- object publish 後
- manifest file flush 後
- manifest publish 後
- inventory publish 前 / 後
- snapshot root publish 前 / 後
- refs/latest replace 前 / 後
- commit state publish 前 / 後
- journal cleanup 前 / 中 / 後

### 7.3 restore / discard fault points

- transaction journal publish 後
- staged data file flush 後
- backup file flush 後
- backup publish 後
- working tree rename / replace 前 / 後
- 64-file batch 境界の前 / 後
- parent barrier 前 / 後
- transaction committed state publish 前 / 後
- backup / staged / journal cleanup 中

### 7.4 registry / maintenance fault points

- registry temporary write 中
- registry file flush 後
- registry replace 前 / 後
- registry parent barrier 前 / 後
- GC apply の各 move / delete batch 前後
- checkpoint delete のinventory / refs / journal更新前後
- index rebuild 中。ただしindexは再構築可能データとして扱う

### 7.5 再起動後の合格条件

各 crash 後、手動でファイルを削除・コピーせず、次を実行する。

1. CheckPo を通常起動
2. `status`
3. transaction list / 自動 recovery
4. quick verify
5. full verify
6. index rebuild
7. GC analyze
8. working tree と対象 checkpoint の hash / manifest 比較

合格状態:

- 操作が commit 前なら旧状態、commit 後なら新状態として一意に確定する
- recovery 後に中途半端な working tree、参照切れ、silent data loss がない
- success を返した操作が再起動後に消えていない
- registry は旧版または新版の妥当な JSON で、missing / zero-byte / partial JSON にならない
- transaction は自動回復または安全な quarantine となり、ユーザーの元データを残す
- cleanup warning が残る場合も、理由と安全なUI導線があり、無限 retry しない
- 同じ recovery を2回実行しても状態が変わらない

## 8. Windows 実利用互換性

### 8.1 path / filesystem

- ASCII、ひらがな、漢字、絵文字、結合文字を含む path
- 大文字小文字だけが異なる名前
- 260文字を超える long path
- Windows reserved name、末尾 dot / space、colon、backslash を安全に拒否
- junction、symlink、mount point、reparse point を追跡しない
- project root または storage root の途中 component を junction に差し替える競合
- repository を project 内へ設定できない
- project と repository が別 volume の場合の動作または明示的拒否
- removable drive の切断、read-only volume、容量不足

### 8.2 ACL / file lock

- standard userで read-only file / directory
- ACL で delete、write-data、write-attributesの一部だけ拒否
- Unity Editorがfileを開いている
- 別processがshare-deleteなしでfileを開いている
- Defender / indexer が一時的にhandleを保持している
- retry可能エラーと恒久エラーを区別し、無限待機しない
- 失敗時に元fileを削除せず、backupも勝手に消さない

### 8.3 並行実行・キャンセル

- CLIを2プロセス同時起動し、repository lockで片方を安全に拒否
- GUIとCLIを同時操作
- checkpoint中にUnityが対象fileを更新
- restore / discard中に対象fileを更新・差し替え
- checkpoint / verify / restore のキャンセル
- process終了後にlockが残留しない
- sleep / resume、ユーザーログオフ、Windows Update再起動

## 9. 性能測定

### 9.1 測定方法

- 正式値は release build、標準ユーザー、Defender有効で取得する
- 各操作について warm run を最低5回。cold cache は別系列として最低3回
- 他の重いprocessを止め、power schemeを固定する
- wall time、Core phase timing、CPU、peak memory、read/write bytes、handle open数を記録する
- ProcMon / Windows Performance Recorder は診断 run だけで使い、計測 overhead のない正式値と分ける
- macOS比較を行う場合は、同等CPU・同等SSD・同じproject・同じ操作順を使う
- Windows / NTFS 固有の差と断定せず、hardware / VM / Defenderを含む環境差として報告する

### 9.2 必須計測

- 初回 checkpoint のphase別時間
- checkpoint 2-100 / 101-500 / 501-1000 のmedian / p95 / p99 / max
- status / diff / quick verify / index rebuild / GC @100 / 500 / 1000
- full verify @1000
- 20,000 files add / diff / checkpoint / delete / restore / cleanup / verify
- 2 GiB add / diff / checkpoint / restore / mtime-only restore / cleanup / verify
- handle open、FileId取得、fingerprint取得、hash、readback、file flush、directory barrierの件数と時間

### 9.3 暫定性能ゲート

製品要件が確定するまで、次を暫定値とする。correctness / durability を削って達成してはならない。

| 操作 | 暫定目標 |
|---|---:|
| 30k files 増分 checkpoint median | 5秒以下 |
| 30k files 増分 checkpoint p95 | 8秒以下 |
| status @1000 | 1秒以下 |
| diff latest @1000 | 3秒以下 |
| quick verify @1000 | 30秒以下 |
| full verify @1000 | 120秒以下 |
| 20,000 files checkpoint | 180秒以下 |
| 20,000 files restore | 240秒以下 |
| 2 GiB checkpoint | 60秒以下 |
| 2 GiB restore | 60秒以下 |

目標未達時は、最低限次をWPR / WPAまたはProcMonで分類する。

- repeated CreateFile / NtCreateFile
- GetFileInformationByHandle / FileId enumeration
- Defender filter driver
- file flush / write-through
- directory enumeration
- SQLite fingerprint DB
- object hash / readback
- VM host I/O wait

## 10. GUI / 配布物

CLI合格後に release installer / GUI で確認する。公式releaseのWindows installerはNSISであり、MSIはローカル検証・組織内配布向けに生成できても、現行release workflowでは公開しない。Windows Authenticode署名は未設定で、Tauri updaterの署名鍵による更新検証とは別物として扱う。

- installer、初回起動、通常ユーザー権限
- project選択、storage root選択
- checkpoint create / list / rename / delete
- diff表示
- restore / discard preview、確認dialog、apply
- progress、cancel、error表示
- pending transaction / quarantine / registry unavailable の案内
- アプリ再起動後の状態保持
- 日本語・英語表示、long path表示
- Windows Authenticode署名の有無、SmartScreen、Tauri updater署名を含む更新導線
- uninstallしてもrepository / Unity projectを削除しない

CLI build成功だけでGUI合格にしない。主要操作は実画面で行い、スクリーンショットとログを保存する。

## 11. 成果物

runごとに次を保存する。

```text
results/
  environment/
  environment.json
  source-diff.patch
  checkpoint-series.jsonl
  operations.jsonl
  scenario-actions.jsonl
  scenario-summary.jsonl
  fault-injection.jsonl
  recovery-results.jsonl
  api-probes/
  procmon/
  wpr/
  screenshots/
  logs/
  report.md
```

`report.md` には次を先頭に書く。

- 判定: Go / Conditional Go / No-Go
- 対象 commit、CLI SHA、script SHA
- Windows / filesystem / hardware / VM情報
- 手動介入の有無
- failure件数と未解決事項
- durability fault matrixの成功数 / 総数
- 性能ゲートの達成状況
- 正式サポート範囲と非対応filesystem

## 12. 最終チェックリスト

### P0: 正式判定前に必須

- [ ] `NtSetInformationFile` のinformation class、RootDirectory-relative rename、NTSTATUS / Win32 errorの対応をprobeで確定した
- [ ] directory flush の access denied 発生箇所と保証範囲を確定した
- [ ] `CreateFileW` directory handleの最小access / share mode / flagsを確定した
- [ ] registry消失を再現または再発しない根拠を得た
- [ ] journal cleanup mismatchの原因とrecoveryを確認した
- [ ] 単一commit・単一SHA・fresh rootで全E2Eを完走した
- [ ] ops-100 / 500 / 1000をinterleaveして測定した
- [ ] 手動介入0、pending transaction 0、warning 0だった
- [ ] process kill fault matrixを完走した
- [ ] VM hard reset fault matrixを完走した
- [ ] 物理Windows NTFSで主要hard-power試験を完走した
- [ ] same-size / same-mtime変更とobject改ざんを検出した
- [ ] standard user、Defender有効で完走した
- [ ] release jobが実行しないfmt / clippy / frontend testについて、同一commitのCI成功または手動実行結果を保存した

### P1: リリース品質

- [ ] path / Unicode / long path / reparse matrixを完走した
- [ ] ACL / locked file / Unity編集中のケースを完走した
- [ ] concurrent CLI / GUI、キャンセル、sleep / resumeを確認した
- [ ] 20,000 files / 2 GiBシナリオを完走した
- [ ] 性能ボトルネックをWPR / ProcMonで分類した
- [x] GUI主要操作を実画面で確認した
- [ ] installer / update / uninstallを確認した
- [ ] 対応OS・filesystem・既知制限を文書化した

## 判定上の注意

- 「1000 checkpoint作成成功」は正常終了系の機能証明であり、crash durabilityの証明ではない
- APIが成功を返したことは、hard reset後の永続化保証を意味しない
- quick/full verify成功は、そのrunでworking tree変更を取り逃していないことまでは証明しない
- VMでのhard reset成功は、物理電源断試験の代替にならない
- Defender無効時だけの性能は正式値にしない
- fallbackで完走させたbuildは診断用であり、release candidate合格として扱わない
- 手動でregistryやtransactionを修復したrunは、以降の結果が正常でも正式E2E不合格とする
