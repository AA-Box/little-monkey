// The desktop runtime baseline (WebView2/WKWebView/WebKitGTK versions supported
// by Tauri 2) implements ES2021 String.prototype.replaceAll, while this
// project's TypeScript lib target intentionally remains older. Keep the type
// declaration local rather than widening the whole compiler lib target.
interface String {
  replaceAll(searchValue: string | RegExp, replaceValue: string): string;
}
