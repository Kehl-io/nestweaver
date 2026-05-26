import * as vscode from "vscode";
import * as child_process from "child_process";
import { NestWeaverApi } from "./api";
import { GraphWebviewProvider } from "./panels/GraphWebviewProvider";

let serverProcess: child_process.ChildProcess | undefined;

export async function activate(context: vscode.ExtensionContext) {
  const config = vscode.workspace.getConfiguration("nestweaver");
  const port = config.get<number>("port", 3000);
  const api = new NestWeaverApi(`http://127.0.0.1:${port}`);

  // Check server
  try {
    await api.health();
  } catch {
    if (config.get<boolean>("autoStart", true)) {
      const answer = await vscode.window.showInformationMessage(
        "NestWeaver server not running. Start it?", "Yes", "No");
      if (answer === "Yes") {
        const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath || ".";
        serverProcess = child_process.spawn("nestweaver",
          ["ui", "--no-open", "--port", String(port), "--db", `${root}/nestweaver.lbug`],
          { stdio: "ignore", detached: true });
        serverProcess.unref();
        for (let i = 0; i < 10; i++) {
          await new Promise(r => setTimeout(r, 1000));
          try { await api.health(); break; } catch {}
        }
      }
    }
  }

  const graphProvider = new GraphWebviewProvider(context.extensionUri, api);
  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider("nestweaver.graphView", graphProvider));

  context.subscriptions.push(
    vscode.commands.registerCommand("nestweaver.showGraph", () =>
      vscode.commands.executeCommand("nestweaver.graphView.focus")));

  context.subscriptions.push(
    vscode.commands.registerCommand("nestweaver.showInGraph", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) return;
      const word = editor.document.getText(
        editor.document.getWordRangeAtPosition(editor.selection.active));
      if (word) {
        try {
          const results = await api.search(word, 5);
          if (results.length > 0) graphProvider.focusOnNode(results[0].uid);
          else vscode.window.showInformationMessage(`No symbols for "${word}"`);
        } catch { vscode.window.showErrorMessage("NestWeaver search failed"); }
      }
    }));
}

export function deactivate() {
  serverProcess?.kill();
  serverProcess = undefined;
}
