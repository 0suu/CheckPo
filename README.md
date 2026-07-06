# CheckPo Local

CheckPo Local は、Unity / VRChat 向けのローカル checkpoint / diff / restore / discard ツールです。Git の代替ではありません。branch、merge、conflict などの Git 概念をユーザーに見せず、Unity プロジェクト内の安全な範囲だけを保存・比較・巻き戻します。

## 安全境界

操作対象はファイルパスとしての次の範囲だけです。

```text
Assets/**
Packages/**
ProjectSettings/**
```

`Assets`、`Packages`、`ProjectSettings` というディレクトリ自体は操作対象ではありません。`README.md`、`.git/`、`Library/`、`Temp/`、`Logs/`、`UserSettings/`、`.checkpo/`、絶対パス、`..` を含むパス、backslash を含むパスは拒否します。symlink は checkpoint 作成時に追跡せず、restore / discard では通常ファイルとして辿りません。

Core の破壊的操作は `TrackedUnityFilePath` だけを受け取り、CLI/Tauri から来た文字列は境界で即 validation します。UI の disabled 状態には安全性を依存しません。

## 保存設計

プロジェクト内には path-free marker だけを置きます。

```text
<Unity Project>/.checkpo/project.json
```

marker には `schemaVersion`、`projectId`、`createdAtUtc` だけを保存します。project root や storage root の絶対パスは入れません。

`projectId` は checkpoint lineage の ID です。Unity project の物理 path は identity ではなく、移動・リネームされる前提で扱います。storage root は user data dir 側の registry で管理します。checkpoint の正本は external storage の `snapshots/` と `objects/` です。SQLite は再構築可能な derived index であり、壊れても checkpoint / restore / discard の正本にはしません。

registry には最後に確認された project root を保存します。前回 path と現在 path が違う場合、前回 path に同じ `projectId` の marker が残っていなければ移動・リネームとして現在 path を採用できます。前回 path に同じ `projectId` の marker が残っている場合はコピー疑いとして扱い、checkpoint 作成、削除、restore / discard apply、GC apply などの変更操作は Core 側で拒否します。ユーザーは「この場所を使う」か「別プロジェクトとして開始」を選びます。

GUI の設定画面から checkpoint 保存先を変更できます。この操作は registry の保存先だけを更新し、既存 checkpoint ファイルはコピー・移動・削除しません。保存先を変える場合は、先に現在の storage root の `repos/<project-id>/` を新しい storage root の `repos/<project-id>/` へ手動で移動してください。移動先に同じ project id の repository がない場合、保存先変更は拒否されます。

```text
<storage-root>/
  registry.json
  repos/<project-id>/
    repo.json
    refs/latest
    snapshots/<snapshot-id>.json
    objects/loose/ab/cd/<object-id>
    indexes/local.db
    journals/<transaction-id>/
```

snapshot id と object id は BLAKE3 の 64 文字 lowercase hex です。snapshot は canonical JSON bytes の BLAKE3、object は whole-file bytes の BLAKE3 です。

## 破壊的操作

`restore` は working tree 全体を指定 checkpoint に戻します。`discard` は指定した tracked file path だけを checkpoint に戻します。

どちらも transaction journal を通します。

- apply 前に preview 時点の hash / 存在状態を再確認する。
- Restore / Replace 用 object は `staged/` に展開し、hash / size を検証する。
- Replace / Delete 対象の現在ファイルは削除せず `backup/` に move する。
- Restore / Replace 後は snapshot の `modifiedAtUtc` を file mtime に復元する。
- pending transaction がある場合、新しい mutating operation は拒否する。

## CLI

```bash
checkpo init <project-path> [--json]
checkpo init <project-path> --start-as-separate [--json]
checkpo status <project-path> [--json]

checkpo checkpoint create <project-path> --name <name> [--init-if-needed] [--json]
checkpo checkpoint list <project-path> [--json]
checkpo checkpoint delete <project-path> <checkpoint-id> --yes [--json]

checkpo diff <project-path> --checkpoint <checkpoint-id> [--json]

checkpo restore preview <project-path> --checkpoint <checkpoint-id> --json > restore-plan.json
checkpo restore apply <project-path> --checkpoint <checkpoint-id> --expected-plan restore-plan.json --yes [--json]

checkpo discard preview <project-path> --path <tracked-path> [--path <tracked-path>...] [--checkpoint <checkpoint-id>] --json > discard-plan.json
checkpo discard apply <project-path> --path <tracked-path> [--path <tracked-path>...] [--checkpoint <checkpoint-id>] --expected-plan discard-plan.json --yes [--json]

checkpo verify <project-path> [--checkpoint <checkpoint-id>] [--quick] [--json]
checkpo index rebuild <project-path> [--json]
checkpo storage gc analyze <project-path> [--json]
checkpo storage gc apply <project-path> --yes [--json]
checkpo storage set-root <project-path> --storage-root <path> --yes [--json]
checkpo transactions list <project-path> [--json]
checkpo transactions recover <project-path> [--json]
checkpo maintenance cleanup-journals <project-path> [--json]
```

unknown option は `clap` で error になります。旧クラウド出力、別フォルダ復元、旧巻き戻し互換 command はありません。

## 開発

```bash
cargo fmt --all --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets -- -D warnings
```

このリポジトリは未リリース段階です。旧 marker / repository / snapshot schema との互換や migration はありません。下方互換性維持だけの分岐、fallback、移行コードは入れません。
