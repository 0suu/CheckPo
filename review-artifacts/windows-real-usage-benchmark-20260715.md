# CheckPo Windows 最終レビュー・実利用ベンチ（2026-07-15）

## 結論

**通常終了系の Windows / NTFS release candidate と Milfy 規模ベンチは Go。**

- ZIP候補を現行worktreeへ全面適用し、リポジトリ規約で変更禁止の `.github/workflows/release.yml` だけは旧内容を維持した。
- multi-lens reviewで見つかったWindows安全性のP0/P1を修正し、最終再レビューは残存P0/P1なし。
- `fmt`、workspace全test、全target clippy、frontend 41 testが成功した。
- 30,291 source files / 7,649,982,084 bytesを隔離copyし、1000連続checkpoint、20,000 small files、2 GiB file、clean anchorを含む最終1004 checkpointまで完走した。
- 最終statusはindex current、diff 0、pending transaction 0、unresolved quarantine 0、warning 0。GC後のunreferenced object/chunk 0、full verify valid。
- Tauri release GUIを実画面で網羅操作し、自動復元、履歴描画、正確な差分、選択巻き戻し、過去/最新への全体復元、チェックポイント削除、transaction cleanup、GC、破損チェック、設定、テーマ、通常終了、処理中キャンセル終了を確認した。
- 実画面テストで見つかったindex欠損時のnull参照、モーダル背面に隠れるエラー、通常ダイアログをEscで閉じられない問題を修正し、release再build後に再現手順で合格した。

ただし、これは正式リリース条件の全完了ではない。物理Windows 11でのhard-power-off反復とWindows Server VM hard resetは今回未実施で、正式リリース前の別ゲートとして残る。

## 候補版と固定環境

- ZIP: `CheckPo-PostReview-Fixes-20260715.zip`
- ZIP SHA-256: `BADA1D9C29E1898A1CA6BC62CCCE8DD37FDFDB1C2399A7F37F7261FD088D2FA2`
- ZIP適用: 97 files、missing 0、hash mismatch 0
- 保護ファイル: `.github/workflows/release.yml` は変更なし
- 実行OS: Windows `10.0.26200` / AMD64
- filesystem: D: / NTFS / volume `SN770`
- release CLI: `target/release/checkpo.exe`
- 実行時CLI SHA-256: `9A470DA35E74936DAB6E983D7C239D2509DEBD4456A4CB18E84AE2E9A567B707`
- 実行時benchmark script SHA-256: `D3950C867103A22AAB215106116A2A9739BF839B2E4BFFA53ACE45658E043B0E`
- benchmark後のallocated-bytes修正版script SHA-256: `7916E7390D3C5616551ED5FC3011A70C190892DF6E94B9B2AE8B7FC0B2C13F49`
- benchmark root: `D:\git\suu\CheckPo\target\benchmark-milfy-20260715-final`
- raw results: `D:\git\suu\CheckPo\target\benchmark-milfy-20260715-final\results`
- raw CLI operations: 1,092、non-zero exit 0
- checkpoint series: 1..1000連続、ID欠落・重複なし

sourceは次のtracked rootsだけを`robocopy /COPY:DAT /DCOPY:DAT /XJ`で隔離copyした。benchmarkの書き込み先はbenchmark root配下だけである。

| 対象 | files | logical bytes |
|---|---:|---:|
| source `Assets/Packages/ProjectSettings` | 30,291 | 7,649,982,084 |
| copy直後（sentinel除外） | 30,291 | 7,649,982,084 |
| 最終project（19-byte sentinel込み） | 30,292 | 7,649,982,103 |

copyは24.5秒だった。sourceのfiles/bytesはベンチ後も同値だった。

## レビューで修正したリリース阻害事項

### 1. Windows relative rename / directory durability

- Win32 `FILE_RENAME_INFO` とkernel向け `FILE_RENAME_INFORMATION` の契約混同をやめ、保持親handleを `RootDirectory` とする `NtSetInformationFile` relative renameへ変更した。
- directory mutation handleはpathを無条件に信用せず、既存anchorと128-bit FileIdを照合したwrite-through twinだけを使う。
- renameのcommit後にreadback/flushが失敗し得るため、error pathでもsource/destination両parent barrierを試行する。
- directory barrier失敗を成功扱いするfallbackは追加していない。

### 2. 条件付きreplaceの誤削除とcrash recovery

- destination検証後に第三者が別fileを挿入した場合、`REPLACE_IF_EXISTS`でそのfileを消し得た競合を修正した。
- verified old destinationをprivate tombstoneへno-replace renameし、newもno-replaceでpublishする二段階protocolへ変更した。
- old detach前に `{destination leaf, temporary leaf, tombstone leaf, old/new FileId}` のrecovery recordをfile sync + parent syncする。
- crash状態はFileIdで判定し、destination欠落時はold tombstoneをno-replace rollbackする。unknown FileIdは何も削除せず失敗する。
- destinationがold/newに見えても、parent sync成功前に反対側のcopyを削除しない。
- finalize時はrename用DELETE handleをdrop後、同じFileIdを `FILE_SHARE_DELETE` なしで再openし、そのguardをdestination barrier、old tombstone削除、record削除まで保持する。
- recovery record durable直後、old detach直後、new publish直後、finalize競合をWindows実filesystem testで再現した。

### 3. 同一FileIdのin-place書換え

- Windows `FileVersion` とfingerprint v3へ `FILE_BASIC_INFO.ChangeTime` と128-bit FileIdを追加した。
- project backup開始からversioned deleteまで `FILE_SHARE_WRITE` なしのguardを保持し、同一size・mtime復元を伴うin-place書換えも拒否する。
- Windowsもproject fileを直接backupへmoveせず、独立copyをpublish・mtime復元・sync・hash/readback後に元fileをversioned identity-bound deleteする。
- read-only project fileのdiscard E2Eを追加した。

### 4. その他

- NTFSのcase-insensitive照合を `CompareStringOrdinal(..., ignoreCase=true)` と実filesystem testで統一した。
- 未対応platformのno-replace renameは `exists()+rename` を廃止し、`Unsupported` でfail-closedにした。
- Tauri test binaryのloader hangを解消し、close decisionをpure helper化してidle/cancellable/non-cancellableをunit testした。

最終のread-only再レビュー判定は **残存P0/P1なし**。残存P2は、destinationが存在するcrash状態ではreplace record/tombstone cleanupが次回同一leaf replaceまで遅れる場合があること。old/newの正本安全性には影響しない。

## 自動検証

| 検証 | 結果 |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --workspace --all-targets --locked -- -D warnings` | pass |
| `cargo test --workspace --locked` | pass |
| CLI unit | 8 pass |
| CLI maintenance integration | 9 pass |
| Core lib | 135 pass / 4 ignored |
| Core E2E | 136 pass |
| storage root integration | 11 pass |
| Tauri unit | 9 pass |
| frontend `node --test frontend-state.test.js` | 41 pass |

ignored 4件は明示的benchmark/scale test。今回の実利用benchmarkはrelease CLIを使う別driverで実施した。

## 初回checkpoint

初回は外部93.053秒、Core 93.014秒。30,292 files / 7,649,982,103 bytesをhashし、重複排除後6,790,867,636 bytesの29,377 loose objectを新規保存した。

| phase | 時間 |
|---|---:|
| scan total | 18.400 s |
| object store | 66.957 s |
| manifest build | 0.178 s |
| manifest store | 1.762 s |
| final object readback | 5.185 s |
| root/journal/inventory/ref commit | 0.145 s |
| index update | 0.114 s |
| fingerprint update | 0.082 s |

## 1000連続checkpoint

各回は19-byte sentinelだけを変更した。checkpoint 2..1000の999件は、既存30,291 files / 7,649,982,084 bytesをfingerprint再利用している。

| 範囲 | n | mean | p50 | p95 | max |
|---|---:|---:|---:|---:|---:|
| 2–100 | 99 | 4.685 s | 4.078 s | 5.166 s | 21.132 s |
| 101–500 | 400 | 4.160 s | 3.956 s | 4.166 s | 17.977 s |
| 501–1000 | 500 | 4.049 s | 3.949 s | 4.090 s | 19.115 s |
| 2–1000 | 999 | 4.156 s | 3.957 s | 4.196 s | 21.132 s |

p50/p95は履歴数に比例して悪化していない。maxは一時的なI/O spikeで、その後は約4秒へ復帰した。

## milestone操作

| checkpoint数 | status | latest diff | quick verify | index rebuild | GC analyze | full verify |
|---:|---:|---:|---:|---:|---:|---:|
| 100 | 1.176 s | 2.396 s | 4.136 s | 5.420 s | 3.386 s | - |
| 500 | 1.262 s | 2.376 s | 4.297 s | 6.959 s | 4.073 s | - |
| 1000 | 1.374 s | 2.344 s | 19.809 s | 10.357 s | 5.325 s | 13.469 s |
| 1004 final | 2.191 s | 2.395 s | 7.094 s | 12.423 s | 6.907 s | 10.399 s |

100件のGC直後quick verifyで39.278秒の単発spikeがあった。500件以降のGC直後quick verifyは4.474 / 5.253 / 7.054秒だった。

全milestoneでlatest diffはadded/modified/deleted 0、index current、pending/quarantine/warning 0、GC integrity問題0だった。

## 20,000 small files

100 directories / 20,000 files / 1,148,894 bytes。

| 操作 | 時間 | 結果 |
|---|---:|---|
| tree生成1回目 | 5.466 s | 20,000 files |
| 追加diff | 11.557 s | added 20,000 |
| baseline restoreで追加分削除 | 165.035 s | applied、tree空 |
| tree生成2回目 | 5.190 s | 20,000 files |
| 20,000件入りcheckpoint | 77.721 s | 50,292 files、warning 0 |
| working tree全削除 | 1.887 s | 20,000 files |
| 削除diff | 2.580 s | deleted 20,000 |
| 20,000件restore | 109.875 s | files/bytes一致、diff 0 |
| baseline cleanup | 169.780 s | tree空、全体diff 0 |
| quick verify | 7.091 s | valid |

small-files cleanup後に `bench-small-files-cleanup` anchorをdriverの記録APIで作成した。これがないとlatestはsmall-files入りcheckpointのままで、次scenarioのclean preconditionが正しく失敗する。初回large-file起動はこのpreconditionでmutation前に停止し、anchor作成後にfresh scenario stateから再開した。

## 2 GiB file

対象は2,147,483,648 bytes、SHA-256 `bf349dded76b35291fe9f80e2946e6b3160cc08446ea99ee9353281558776d0f`。

| 操作 | 時間 | 結果 |
|---|---:|---|
| 生成・fsync・validation hash | 2.400 s | size/hash一致 |
| 追加diff | 2.768 s | added 1 |
| 追加file discard | 8.662 s | applied、path消滅 |
| 2 GiB入りcheckpoint | 9.005 s | 30,293 files、warning 0 |
| working tree削除 | 0.148 s | 成功 |
| 削除diff | 8.035 s | deleted 1 |
| restore | 8.146 s | size/SHA-256一致、diff 0 |
| mtime-only restore | 5.009 s | metadata 1、replace/stage/backup 0 |
| baseline cleanup | 9.531 s | path消滅、全体diff 0 |
| quick verify | 7.298 s | valid |

実行時driverのraw `allocatedBytes` はctypesのsigned return型により `-2147483648` と誤記録された。logical size、hash、CheckPo結果には未使用。driverをunsigned `c_ulong`へ修正し、保存済み2 GiB objectで `allocatedBytes=2147483648` を確認した。修正後結果を1000系列へ混ぜていないため、実行時script SHAと現行script SHAを分けて記録している。

## 最終状態

small-files present/cleanup、large-file present/cleanupを加えたベンチ系列の最終checkpoint数は1004。

- checkpoint index: current
- checkpoint count: 1004
- unique blob count: 50,377
- stored size: 8,964,206,826 bytes
- aggregate logical snapshot size: 7,682,730,663,954 bytes
- latest diff: 0 / 0 / 0
- pending transactions: 0
- unresolved quarantines: 0
- warnings: 0
- GC after apply: unreferenced blob 0、unreferenced manifest chunk 0、integrity problem false
- full verify: valid、error 0、warning 0

## Tauri GUI 実画面検証

現行sourceからrelease buildした `checkpo-tauri.exe` をWindows実画面で操作した。最終GUI binary SHA-256は `2992E6C4510618A798A106E0ADC02E301024B0AFCE3514B8473B4EE8E68FEB47`。

- 隔離Milfy project / CheckPoDataの登録と次回起動時の自動復元: 成功
- 1005 checkpointのvirtualized履歴表示、検索欄、選択状態: 正常
- 正確な差分確認: added 0 / modified 0 / deleted 0、warning 0
- 選択checkpointの破損チェック: 30,292 files、問題なし。進捗と中止ボタンも表示
- 設定: 現在の保存先、theme切替、GC分析を確認。GCは不要object/chunk 0
- 上級者向け: 復旧済みtransaction cleanup候補 7件 / 40,009 files / 4.0 GBをpreview。一時file候補 0。削除操作は未実行
- 作業記録と詳細JSON表示: 正常
- 通常終了: process残留なし
- 破損チェック実行中の終了: cancellationを経由し597 msでprocess終了。通常完走時の約14秒を待たず終了し、再確認でpending transaction 0

### 追加の網羅GUI試験と修正

専用QA project `target/gui-qa-20260715-exhaustive/UnityProject` と専用storage `StorageB` を使い、正常系、異常系、破壊的操作をrelease GUIから実行した。更新の確認表示までは検証したが、ユーザー指定によりupdaterのインストール操作は実行していない。

実画面テストで次の3件を検出し、multi-lens reviewでcorrectness/accessibility/lifecycle観点を確認してから最小修正した。

1. index DB欠損時に `storage` がnullでも `storedSizeBytes` を代入し、`Cannot set properties of null` で描画が止まる。
2. project登録などのmodal表示中に発生したエラーが背面bannerへ出るため、`aria-modal` / `inert` により読めず閉じられない。
3. 設定、その他、project選択、登録、復元preview、エラーの通常dialogをEscで閉じられない。

修正後は、index DBとworking-tree cacheを退避した実repositoryで索引欠損の日本語案内と「索引を再構築」を表示し、再構築後に3 checkpoint / 2.1 KBを復元した。無効project登録では最前面の`alertdialog`へエラーを表示し、Esc 1回でエラー、Esc 2回で登録dialogを閉じられた。汎用Esc mapには確認dialogと処理中dialogを追加していない。確認dialogは既存handlerでcancelだけを行い、処理中dialogはEsc対象外である。

破壊的操作の実測結果:

| 操作 | preview / 実行結果 | 最終確認 |
|---|---|---|
| 選択分を戻す | `Assets/AddedFromQa.txt` 1件を確認dialog経由で削除 | exact diff 3→2、実path消滅 |
| 初回checkpointへ復元 | 復元1 / 置換1 / 削除0、対象2、約303 B | exact diff 0、`DeletedLater.txt`復元、`Demo.cs` baseline、snapshot mtime復元 |
| 最新checkpointへ復元 | 復元1 / 置換1 / 削除1、対象3、約354 B | exact diff 0、added fileとmodified contentを復元、deleted file消滅、snapshot mtime復元 |
| 中間checkpoint削除 | `GUI QA 改名_日本語_123` を確認dialog経由で削除 | 3 CP→2 CP、使用中保存データ 1.9 KB |
| transaction cleanup | 3 transaction / 7 files / 3.2 KBをpreview後に削除 | 再preview 0件 / 0 files / 0 B |
| 一時file cleanup | preview 0件 / 0 B | apply無効のまま |
| 専用storage GC | object 0 / chunk 0 / 0 B | apply無効、integrity問題なし |
| 作業記録 / 詳細結果 | 表示内容を確認後に各clear | list空、詳細 `{}` |
| 選択 / project全体verify | 両方を実行 | valid、error 0、warning 0 |

大型benchmarkでは、キャンセル到達前に作成が完了してしまった `GUI cancel probe` 1件だけをGUIで削除した。削除後の履歴は1006 CPから1005 CPへ戻り、先頭は元の「初回チェックポイント」、working tree exact diff 0 / unchanged 30,292だった。削除後の8.3 GB storage GCもobject 0 / chunk 0 / 0 B。削除は5秒時点で差分更新中、次の10秒以内にUI操作可能状態へ戻った。

キャンセル試験では大型checkpoint作成が安全なcancel pointへ届く前に完了したため、cancel要求が必ず操作を中断するとは確認できなかった。完了したcheckpointは上記のとおり削除済みで、データ破損、pending transaction、操作不能は発生していない。

異常系では無効Unity project、保存先変更失敗、index欠損/再構築、複製projectのidentity分離、初回checkpointなし登録、検索/フィルタ、window最小化/最大化/復元、light/dark/system themeを確認した。Windows Graphics Captureで操作直後だけ黒frameになることがあったが、accessibility treeとアプリ処理は継続し、次frameで正常化したためアプリ描画障害とは判定していない。

GUI登録時の既定選択により隔離repositoryへ「初回チェックポイント」を1件追加したため、GUI後の正規checkpoint数は1005。キャンセル試験で一時的に1006となったが余分な1件は削除済み。最終GUI再確認は1005 CP、exact diff 0 / 0 / 0、GC 0 Bだった。

実画面で、ライトthemeのdisabled primary buttonが有効色に見える問題を確認した。`.button.primary:disabled`を中立色・shadowなしへ変更し、再build後に「保存先を変更」「チェックポイントを作成」の表示を再確認した。frontend 41 test、`cargo fmt --check`、workspace test、全target clippy、release buildは成功。

## 破壊的変更と残ゲート

- `repo.json` はschema v2 / repository format v5、Snapshot v2、canonical inventoryを要求する。
- 旧marker / repository / snapshot schemaのmigration・fallbackはない。既存旧形式repositoryは互換利用できず、再初期化が必要。
- destructive preview/apply plan、CLI maintenance apply、storage layoutも旧候補との互換を前提にしない。
- `.github/workflows/release.yml` は変更していない。

今回未実施の正式リリースゲート:

1. Windows 11物理機でprocess kill / OS crash / hard-power-offの反復。
2. Windows Server VMでhard reset反復。
3. crash後に残るreplace recovery artifactの自動eager cleanup改善（P2、データ安全性には非阻害）。
4. debug buildだけに公開されるintegration-test support APIのfeature隔離（P2、release binaryには非搭載）。
