import * as vscode from 'vscode';
import { NestWeaverApi } from './api';

export class NestWeaverCodeLensProvider implements vscode.CodeLensProvider {
  private api: NestWeaverApi;

  constructor(api: NestWeaverApi) {
    this.api = api;
  }

  async provideCodeLenses(document: vscode.TextDocument): Promise<vscode.CodeLens[]> {
    const lenses: vscode.CodeLens[] = [];
    const text = document.getText();

    // Simple regex to find function/class definitions
    const patterns = [
      /(?:function|def|fn|func)\s+(\w+)/g,
      /(?:class|struct|interface|trait)\s+(\w+)/g,
      /(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s*)?\(/g,
    ];

    for (const pattern of patterns) {
      let match;
      while ((match = pattern.exec(text)) !== null) {
        const name = match[1];
        const pos = document.positionAt(match.index);
        const range = new vscode.Range(pos, pos);

        try {
          const symbol = await this.api.symbol(name);
          if (symbol) {
            const callers = symbol.callers?.length ?? 0;
            const callees = symbol.callees?.length ?? 0;
            lenses.push(new vscode.CodeLens(range, {
              title: `${callers} callers | ${callees} callees`,
              command: 'nestweaver.showInGraph',
              arguments: [name],
            }));
          }
        } catch {
          // Symbol not found in graph — skip
        }
      }
    }

    return lenses;
  }
}
