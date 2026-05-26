import * as vscode from "vscode";
import { NestWeaverApi } from "../api";

export class GraphWebviewProvider implements vscode.WebviewViewProvider {
  private _view?: vscode.WebviewView;

  constructor(
    private readonly _extensionUri: vscode.Uri,
    private readonly _api: NestWeaverApi,
  ) {}

  resolveWebviewView(webviewView: vscode.WebviewView) {
    this._view = webviewView;
    webviewView.webview.options = { enableScripts: true };
    webviewView.webview.html = this._getHtml();
  }

  focusOnNode(uid: string) {
    this._view?.webview.postMessage({ type: "focusNode", uid });
  }

  private _getHtml(): string {
    return `<!DOCTYPE html>
<html><head><meta charset="UTF-8">
<style>body{margin:0;padding:16px;font-family:var(--vscode-font-family);color:var(--vscode-foreground)}
.status{font-size:12px;color:var(--vscode-descriptionForeground)}</style></head>
<body><h3>NestWeaver Graph</h3>
<p class="status">Connected. Use "Show in Graph" from editor context menu.</p>
<div id="content"></div>
<script>
const vscode=acquireVsCodeApi();
window.addEventListener('message',e=>{
  if(e.data.type==='focusNode')document.getElementById('content').innerHTML='<p>Focused: '+e.data.uid+'</p>';
});
</script></body></html>`;
  }
}
