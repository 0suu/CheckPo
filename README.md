# CheckPo

CheckPo は、Unity / VRChat 向けのローカル checkpoint / diff / restore / discard ツールです。Git の代替ではありません。branch、merge、conflict などの Git 概念をユーザーに見せず、Unity プロジェクト内の安全な範囲だけを保存・比較・巻き戻します。

## 安全境界

操作対象はファイルパスとしての次の範囲だけです。

```text
Assets/**
Packages/**
ProjectSettings/**
```

`Assets`、`Packages`、`ProjectSettings` というディレクトリ自体は操作対象ではありません。`README.md`、`.git/`、`Library/`、`Temp/`、`Logs/`、`UserSettings/`、`.checkpo/`、絶対パス、`..` を含むパス、backslash を含むパス、Windows 予約名、末尾が dot/space のパスは拒否します。symlink は checkpoint 作成時に追跡せず、restore / discard では通常ファイルとして辿りません。

Core の破壊的操作は `TrackedUnityFilePath` だけを受け取り、CLI/Tauri から来た文字列は境界で即 validation します。UI の disabled 状態には安全性を依存しません。

## 保存設計

プロジェクト内には path-free marker だけを置きます。

```text
<Unity Project>/.checkpo/project.json
```

marker には `schemaVersion`、`projectId`、`createdAtUtc` だけを保存します。project root や storage root の絶対パスは入れません。

`projectId` は checkpoint lineage の ID です。Unity project の物理 path は identity ではなく、移動・リネームされる前提で扱います。storage root は user data dir 側の registry で管理します。checkpoint 内容の正本は external storage の Snapshot v2 root、canonical Merkle manifest chunk、loose objectです。`repo.json`、`refs/`、`inventory/`はその形式、最新位置、checkpoint root集合を保持する永続メタデータです。SQLite は再構築可能な derived index であり、壊れても checkpoint / restore / discard の正本にはしません。

registry には最後に確認された project root を保存します。前回 path と現在 path が違う場合、前回 path に同じ `projectId` の marker が残っていなければ移動・リネームとして現在 path を採用できます。前回 path に同じ `projectId` の marker が残っている場合はコピー疑いとして扱い、checkpoint 作成、削除、restore / discard apply、GC apply などの変更操作は Core 側で拒否します。ユーザーは「この場所を使う」か「別プロジェクトとして開始」を選びます。

GUI の設定画面から、手動移動済みの保存データへ再接続できます。この操作は registry の参照先だけを更新し、既存 checkpoint ファイルはコピー・移動・削除しません。先に現在の storage root の `repos/<project-id>/` を新しい storage root の `repos/<project-id>/` へ手動で移動してください。移動先に同じ project id の repository がない場合、再接続は拒否されます。

アプリの更新確認・ダウンロード・適用は初期MVPの対象です。Unity プロジェクトと checkpoint データの保存・比較・復元は引き続きローカルで完結し、更新機能だけがリリース配布先と通信します。

```text
<project-storage-root>/
  repos/<project-id>/
    repo.json
    refs/latest
    refs/checkpoint_names.json
    snapshots/v2/ab/cd/<snapshot-id>.root
    inventory/snapshots/head
    inventory/snapshots/states/ab/<state-id>.state
    inventory/snapshots/sets/roots/ab/<set-root-id>.root
    inventory/snapshots/sets/leaves/ab/<set-leaf-id>.leaf
    manifests/v2/nodes/ab/<manifest-node-id>
    manifests/v2/leaves/ab/<manifest-leaf-id>
    objects/loose/ab/<object-id>
    indexes/                         # 予約済み。SQLite DBは置かない
    journals/transactions/<transaction-id>/
    journals/checkpoint-create/<transaction-id>/
    journals/checkpoint-create/.prepare-<transaction-id>/
    journals/checkpoint-create/.cleanup-<transaction-id>-<nonce>/
    journals/checkpoint-delete/<transaction-id>/
    journals/checkpoint-delete/.prepare-<transaction-id>/
    journals/checkpoint-delete/.cleanup-<transaction-id>-<nonce>/
    journals/transaction-cleanup-trash/<batch-id>/<transaction-id>/
    quarantined-journals/<transaction-id>-<quarantine-id>/
    quarantined-journals/<transaction-id>-<quarantine-id>.json
    quarantined-journals/<transaction-id>-<quarantine-id>.resolved
    recovery-rescues/<transaction-id>/objects/<object-id>
    recovery-rescues/<transaction-id>/records/<plan-id>.json
    recovery-rescues/<transaction-id>/active.json
    tmp/
    locks/
```

registry と再生成可能なSQLite索引は、repository配下ではなくOSのprivate user-dataに保存します。既定のproject storage rootはこのuser-dataと同じ場所ですが、カスタムstorage rootを設定してもregistryは移動しません。

```text
user-data/
  registry.json
  derived-indexes/<projectId>/
    local.db
    working-tree-cache.db
```

snapshot id、manifest chunk id、object id は BLAKE3 由来の64文字 lowercase hexです。snapshot rootとmanifest chunkは、JSONではなくSnapshot v2のcanonical binary codecで符号化されます。snapshot rootはMerkle manifest rootを参照し、manifest node/leafはpath範囲ごとにcontent-address化され、objectはwhole-file bytesを参照します。

checkpoint root集合は、snapshot idの先頭byteで256分割したcanonical leaf、256 slotのset root、世代・親state・操作IDを含むstateをそれぞれcontent-address化して保持します。追加・削除は対象leafとset rootだけをpath-copyし、期待する旧headとtransaction operation IDを必須にします。同じoperationのreplayは結果state IDが完全一致する場合だけ成功扱いにし、immutable leaf/root/stateをdurable保存した後の`inventory/snapshots/head`更新をcommit pointとします。通常の履歴参照・削除は過去stateを辿らず、物理snapshot rootとの全集合照合はverify、GC、index rebuildで行います。

増分checkpointは、今回のsnapshotが参照するunique objectを確認します。integrity cacheの強いfingerprintが一致するobjectは再hashせず、cache miss・metadata変化時だけbytesをhashします。参照objectが欠損・破損していればworking treeのscan済みhashから修復し、修復できなければ新しいrootを公開しません。既存履歴を含むrepository全体の検査は`verify`で行い、quick verifyは構造・存在・sizeを、full verifyはobject bytesのhashまで検証します。

checkpointはscan時点の内容を保存します。scan後にworking treeが変わった場合、既にscan済みの内容でcheckpointを完了し、その変更は作成直後のdiffに残ります。新規・変更objectを実際にcopyしている途中でsourceが変化した場合はhash不一致として作成を中止します。

将来のバックアップ・端末間転送で持ち運ぶ永続データ（portable set）は `repo.json`、`refs/`、`inventory/`、`snapshots/`、`manifests/`、`objects/` です。いずれかを省くとcheckpointを復元できません。user-dataの`registry.json`と`derived-indexes/`は端末ローカルで、後者は再構築可能です。repository内の`journals/`、`quarantined-journals/`、`recovery-rescues/`、`tmp/`、`locks/`もその端末での処理・復旧専用です。`quarantined-journals/`はcheckpointの一部ではありませんが、復旧できなかった作業の`journal`、`backup`、`staged`を保持するため、ユーザーが状態を確認するまで削除しません。`recovery-rescues/`は復旧競合時に退避したobjectと解決planを保持し、外部exportまたは明示cleanupまで残します。

## 破壊的操作

`restore` は working tree 全体を指定 checkpoint に戻します。`discard` は指定した tracked file path だけを checkpoint に戻します。

どちらも transaction journal を通します。

- apply 前に preview 時点の hash / 存在状態を再確認する。
- Restore / Replace 用 object は `staged/` に展開し、hash / size を検証する。
- Replace / Delete 対象の現在ファイルは削除せず `backup/` に退避する。Windowsはheld handleのrename、Unixはheld sourceからclone/copy・hash・backup file fsync・readback・backup parent barrierを完了し、元pathがハッシュ時と同じfile version / identityのままの場合だけidentity-bound unlinkする。大量小fileは64fileごと、32parent directoryごとにbarrierを置き、検査後の同一inode書き込みや同名差し替えを誤って削除しない。
- Restore / Replace 後は snapshot の `modifiedAtUtc` を file mtime に復元する。
- pending transaction がある場合、新しい mutating operation は拒否する。
- Restore / Replace の staging は bounded parallel でcopy・hash・file fsyncまで行い、全workerとparent directory barrierの完了後だけ `Staged` へ進む。working treeを書き換えるbackup / applyは直列のままにする。
- 自動復旧できない transaction は、明示確認後に全体を `quarantined-journals/` へ移動できる。Unity プロジェクト内のファイルはこの隔離操作では変更せず、backup / staged も削除しない。隔離時に処理前状態を確認できなかった場合は、再起動後も警告を表示し、既知のcheckpointへの全体restoreが完了するまで新規checkpoint作成・削除・discardなどの変更操作を停止する。

## CLI

```bash
checkpo init <project-path> [--json]
checkpo init <project-path> --start-as-separate --yes [--json]
checkpo status <project-path> [--json]

checkpo checkpoint create <project-path> --name <name> [--init-if-needed] [--timings] [--json]
checkpo checkpoint list <project-path> [--json]
checkpo checkpoint delete <project-path> <checkpoint-id> --yes [--json]
checkpo checkpoint rename <project-path> <checkpoint-id> --name <name> [--json]

checkpo diff <project-path> --checkpoint <checkpoint-id> [--json]

checkpo restore preview <project-path> --checkpoint <checkpoint-id> --json > restore-plan.json
checkpo restore apply <project-path> --checkpoint <checkpoint-id> --expected-plan restore-plan.json --yes [--json]

checkpo discard preview <project-path> --path <tracked-path> [--path <tracked-path>...] [--checkpoint <checkpoint-id>] --json > discard-plan.json
checkpo discard apply <project-path> --path <tracked-path> [--path <tracked-path>...] [--checkpoint <checkpoint-id>] --expected-plan discard-plan.json --yes [--json]

checkpo verify <project-path> [--checkpoint <checkpoint-id>] [--quick] [--json]
checkpo index rebuild <project-path> [--json]
checkpo storage gc analyze <project-path> --json > gc-plan.json
checkpo storage gc apply <project-path> --expected-plan gc-plan.json --yes [--json]
checkpo storage set-root <project-path> --storage-root <path> --yes [--json]
checkpo transactions list <project-path> [--json]
checkpo transactions recover <project-path> [--json]
checkpo transactions conflicts analyze <project-path> <transaction-id> [--json]
checkpo transactions conflicts apply <project-path> <transaction-id> --expected-plan <plan.json> --path <relative-path> --export-root <directory> --yes [--json]
checkpo transactions conflicts apply <project-path> <transaction-id> --expected-plan <plan.json> --without-export --yes [--json]
checkpo transactions quarantine <project-path> <transaction-id> --yes [--json]
checkpo maintenance cleanup-journals analyze <project-path> --json > cleanup-plan.json
checkpo maintenance cleanup-journals apply <project-path> --expected-plan cleanup-plan.json --yes [--json]
checkpo maintenance temp-files analyze <project-path> --json > temp-files-plan.json
checkpo maintenance temp-files apply <project-path> --expected-plan temp-files-plan.json --yes [--json]
```

unknown option は `clap` で error になります。旧クラウド出力、別フォルダ復元、旧巻き戻し互換 command はありません。

## クラウド対応について

現行版はクラウドバックアップ・同期に対応しません。Dropbox、Google Drive、OneDrive などの同期フォルダを storage root に指定し、複数端末から同時利用する運用も非対応です。ファイル同期サービスは CheckPo の repository lock や可変な `refs/` を協調制御しないため、競合や途中状態を作る可能性があります。

将来のクラウド対応は、上記のportable setだけを専用APIで転送し、端末ローカルの `registry.json`、`derived-indexes/`、`journals/`、`quarantined-journals/`、`recovery-rescues/`、`tmp/`、`locks/` を同期しない方式で追加します。content-addressedなSnapshot v2 root、manifest chunk、object、inventory state/set root/set leafはimmutable blobとして転送できます。可変な`inventory/snapshots/head`と`refs/`にはクラウド側のcompare-and-swap、競合解決、公開順序が別途必要であり、現行版はクラウド同期そのものを実装していません。

## Windows 配布

公式リリースのWindows installerはNSISです。MSIはローカル検証・組織内配布向けに生成できますが、現行のrelease workflowはMSIを公開しません。どちらのinstallerもWindows Authenticode署名は未設定です。更新機能の署名はTauri updaterの署名鍵で別途検証します。

## 診断ログ

アプリとCLIは、ユーザーデータ領域の `diagnostic-logs/` に日次ログを保存し、直近約1週間分を保持します。操作名、エラー種別、checkpoint / transaction ID、ローカルpathを記録する場合がありますが、Unityファイルの内容は記録しません。GUIの詳細画面から診断ログフォルダを開けます。

## 開発

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
```

このリポジトリは未リリース段階です。旧 marker / repository / snapshot schema との互換や migration はありません。下方互換性維持だけの分岐、fallback、移行コードは入れません。
