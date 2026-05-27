import * as vscode from "vscode";
import * as child_process from "child_process";
import { NestWeaverApi } from "./api";
import { GraphWebviewProvider } from "./panels/GraphWebviewProvider";
import { NestWeaverCodeLensProvider } from "./codeLens";
import { registerChatParticipant } from "./chatParticipant";
import { NestWeaverStatusBar } from "./statusBar";

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
    vscode.commands.registerCommand("nestweaver.showInGraph", async (nameArg?: string) => {
      let word = nameArg;
      if (!word) {
        const editor = vscode.window.activeTextEditor;
        if (!editor) { return; }
        word = editor.document.getText(
          editor.document.getWordRangeAtPosition(editor.selection.active));
      }
      if (word) {
        try {
          const results = await api.search(word, 5);
          if (results.length > 0) { graphProvider.focusOnNode(results[0].uid); }
          else { vscode.window.showInformationMessage(`No symbols for "${word}"`); }
        } catch { vscode.window.showErrorMessage("NestWeaver search failed"); }
      }
    }));

  // CodeLens
  const codeLensProvider = new NestWeaverCodeLensProvider(api);
  context.subscriptions.push(
    vscode.languages.registerCodeLensProvider(
      ['javascript', 'typescript', 'python', 'rust', 'go', 'java', 'c', 'cpp', 'csharp', 'kotlin', 'php', 'ruby', 'dart', 'swift'],
      codeLensProvider
    )
  );

  // Chat participant
  registerChatParticipant(context, api);

  // Status bar
  const statusBar = new NestWeaverStatusBar(api);
  context.subscriptions.push(statusBar);

  // Status action command
  context.subscriptions.push(
    vscode.commands.registerCommand('nestweaver.statusAction', async () => {
      const choice = await vscode.window.showQuickPick(
        ['Re-index repository', 'Run setup', 'Open graph'],
        { placeHolder: 'NestWeaver Actions' }
      );
      if (choice === 'Re-index repository') {
        vscode.window.createTerminal('NestWeaver').sendText('nestweaver index --repo .');
      } else if (choice === 'Run setup') {
        vscode.window.createTerminal('NestWeaver').sendText('nestweaver setup');
      } else if (choice === 'Open graph') {
        vscode.commands.executeCommand('nestweaver.showGraph');
      }
    })
  );
}

export function deactivate() {
  serverProcess?.kill();
  serverProcess = undefined;
}
