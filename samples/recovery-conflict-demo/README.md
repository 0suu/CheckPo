# Unity上書き中の復旧デモ

CheckPo の復元中断後に Unity が同じファイルを保存した状態を、安全に再現するためのサンプル生成ツールです。コミット前に中断したtransactionを処理前状態へ戻し、Unityの保存内容を競合として退避できることを確認できます。

完成済みのプロジェクトをコピーして使う方式ではありません。CheckPo のプロジェクトIDと保存領域はPCごとに登録されるため、`prepare.ps1` が毎回新しいデモを生成して、このPCのCheckPoへ登録します。

## 作成

リポジトリのルートから実行します。

```powershell
.\samples\recovery-conflict-demo\prepare.ps1
```

生成先は `samples/recovery-conflict-demo/generated/RecoveryConflictDemo-日時-ID/` です。

- `UnityProject/`: CheckPoで選択するフォルダー
- `CheckPoStorage/`: このデモ専用のチェックポイント保存領域

既存のUnityプロジェクトやチェックポイントは変更しません。生成のたびに別のプロジェクトIDとフォルダーを使用します。

## 動作確認

1. CheckPoを起動します。
2. スクリプトが表示した `UnityProject` フォルダーを選択します。
3. 上部に表示される「復旧する」を押します。
4. 残したい競合ファイルを選択し、保存先を指定します。
5. 復旧が完了したことを確認します。

`DemoAvatar.prefab` と `.meta` は、`unity-save-after-crash` 版ではなくtransaction開始前の `working-version-before-crash` 版へ戻ります。

選択した `unity-save-after-crash` 版は、rollback前に指定した保存先へコピーされます。復旧中はCheckPo内部の `recovery-rescues` にも保護されます。

## 作り直す

もう一度 `prepare.ps1` を実行してください。既存デモを上書きせず、新しいデモを作成します。
