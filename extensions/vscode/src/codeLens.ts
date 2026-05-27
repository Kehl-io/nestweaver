import * as vscode from 'vscode';
import { NestWeaverApi } from './api';

export class NestWeaverCodeLensProvider implements vscode.CodeLensProvider {
  private api: NestWeaverApi;
  private cache = new Map<string, vscode.CodeLens[]>();

  constructor(api: NestWeaverApi) {
    this.api = api;
  }

  async provideCodeLenses(document: vscode.TextDocument, token: vscode.CancellationToken): Promise<vscode.CodeLens[]> {
    const key = `${document.uri.toString()}:${document.version}`;
    if (this.cache.has(key)) {
      return this.cache.get(key)!;
    }

    const lenses: vscode.CodeLens[] = [];
    const text = document.getText();

    // Simple regex to find function/class definitions
    const patterns = [
      /(?:function|def|fn|func)\s+(\w+)/g,
      /(?:class|struct|interface|trait)\s+(\w+)/g,
      /(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s*)?\(/g,
    ];

    for (const pattern of patterns) {
      if (token.isCancellationRequested) {
        break;
      }
      let match;
      while ((match = pattern.exec(text)) !== null) {
        if (token.isCancellationRequested) {
          break;
        }
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

    this.cache.set(key, lenses);
    // Evict old entries
    if (this.cache.size > 50) {
      const oldest = this.cache.keys().next().value;
      if (oldest) {
        this.cache.delete(oldest);
      }
    }
    return lenses;
  }
}
