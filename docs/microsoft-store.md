# Microsoft Storeへの掲載

栞のStore版は、x64版とARM64版を含む1つの `.msixbundle` として提出します。WebView2が標準搭載されるWindows 11を対象とします。Microsoft Storeが配信用のパッケージに署名します。 [MSIXの提出要件](https://learn.microsoft.com/ja-jp/windows/apps/publish/publish-your-app/msix/app-package-requirements) · [WebView2ランタイム](https://learn.microsoft.com/ja-jp/microsoft-edge/webview2/concepts/evergreen-vs-fixed-version)

## 製品の登録

1. [Microsoft Store開発者登録](https://storedeveloper.microsoft.com/) からアカウントを作成し、Partner Centerで本人確認または法人確認を完了します。
2. Partner Centerの「アプリとゲーム」でMSIXアプリを作成し、掲載する名前を予約します。
3. 製品管理の「製品 ID」を開き、`Package/Identity/Name`、`Package/Identity/Publisher`、`Package/Properties/PublisherDisplayName` を控えます。 [製品IDを表示する](https://learn.microsoft.com/ja-jp/windows/apps/publish/view-app-identity-details)

## バンドルの作成

Windows SDKの `MakeAppx.exe` と、同じリリースから生成したx64・ARM64の `shiori-mde.exe` を使います。両方の実行ファイルは `npm run build:x64` と `npm run build:arm64` で生成できます。SDKを別の場所に入れた場合は `SHIORI_MAKEAPPX` に `MakeAppx.exe` の絶対パスを設定します。

以下の形式で `.build-tools/store-identity.json` を作り、`name`、`publisher`、`publisherDisplayName` に製品IDの値をそのまま転記します。`displayName` には予約済みの名前を指定します。`version` は4つの数値をピリオドで区切り、末尾を `0` にします。更新時は前回より大きいバージョンを指定します。 [パッケージのバージョン規則](https://learn.microsoft.com/ja-jp/windows/apps/publish/publish-your-app/msix/app-package-requirements#package-version-numbering)

```json
{
  "name": "製品IDのName",
  "publisher": "製品IDのPublisher",
  "publisherDisplayName": "製品IDのPublisherDisplayName",
  "displayName": "予約したアプリ名",
  "version": "2.0.0.0"
}
```

```powershell
npm run package:store -- .build-tools/store-identity.json `
  src-tauri/target/x86_64-pc-windows-msvc/release/shiori-mde.exe `
  src-tauri/target/aarch64-pc-windows-msvc/release/shiori-mde.exe
```

`CARGO_TARGET_DIR` を指定してビルドした場合は、その出力先の実行ファイルを渡します。生成した `.msixbundle` は `src-tauri/target/store/` に出力されます。

## Partner Centerへの提出

1. 申請の「パッケージ」に、製品IDを組み込んだ `.msixbundle` をアップロードします。 [MSIXのアップロード](https://learn.microsoft.com/ja-jp/windows/apps/publish/publish-your-app/msix/upload-app-packages)
2. 日本語のストア掲載情報に説明とデスクトップのスクリーンショットを登録します。[栞のスクリーンショット](store-screenshot.png) は1600 × 1000ピクセルのPNGです。 [ストア掲載情報](https://learn.microsoft.com/ja-jp/windows/apps/publish/publish-your-app/msix/add-and-edit-store-listing-info) · [画像要件](https://learn.microsoft.com/ja-jp/windows/apps/publish/publish-your-app/msix/screenshots-and-images)
3. [プライバシーポリシー案](store-privacy.md) を公開用のページに掲載し、そのHTTPS URLとサポート連絡先を入力します。 [MSIX申請の確認項目](https://learn.microsoft.com/en-us/windows/apps/publish/store-readiness-checklist)
4. 配信先と価格を設定し、年齢区分の質問に回答します。「制限付き機能」の `runFullTrust` の理由欄には、下記の承認理由を入力します。登録内容を確認して審査に送ります。

### 掲載文案

栞（しおり）は、Windows向けのMarkdownエディタです。Markdownとプレビューを並べて編集したり、WYSIWYGで文章を見た目のまま編集したりできます。見出し一覧と文書内検索で移動でき、Mermaidの図やコードの構文ハイライトに対応しています。書いた文書はMarkdownで保存し、PDFまたはHTMLに書き出せます。

### runFullTrust の承認理由

```text
ShioriMDE（栞）は、Tauriで開発したWindows向けのMarkdownエディタです。MSIXに含まれるWin32実行ファイル shiori-mde.exe を Windows.FullTrustApplication として起動する構成のため、runFullTrust が必要です。アプリはサインイン中のユーザーの通常のデスクトップ権限で動作します。

Windowsのファイル選択ダイアログやエクスプローラーからユーザーが指定した Markdown ファイルを開き、編集内容を元のファイルまたはユーザーが選んだ保存先に書き込みます。文書と同じフォルダーやユーザーが参照を許可した場所の画像を読み込み、指定された保存先にHTMLやPDFを書き出します。エディタとプレビューの表示、PDFの生成にはWebView2を使用します。

審査時は、ローカルの .md ファイルを開いて編集し、「名前を付けて保存」で保存できます。右上の「エクスポート」からHTMLまたはPDFへの書き出しを確認できます。
```

### 審査用メモ

Shiori is a desktop Markdown editor that runs as the signed-in user. It uses `runFullTrust` to display native Windows file dialogs, open and save documents selected by the user, and export those documents to HTML or PDF. To test it, launch the app, open a local `.md` file, edit text, save a copy, and export it as HTML or PDF.
