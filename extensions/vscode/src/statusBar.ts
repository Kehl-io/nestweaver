import * as vscode from 'vscode';
import { NestWeaverApi } from './api';

export class NestWeaverStatusBar {
  private item: vscode.StatusBarItem;
  private api: NestWeaverApi;
  private timer: NodeJS.Timeout | undefined;

  constructor(api: NestWeaverApi) {
    this.api = api;
    this.item = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
    this.item.command = 'nestweaver.statusAction';
    this.item.show();
    this.refresh();
    this.timer = setInterval(() => this.refresh(), 60000);
  }

  async refresh() {
    try {
      const health = await this.api.health();
      if (health.status === 'ok') {
        this.item.text = '$(check) NestWeaver';
        this.item.tooltip = 'NestWeaver: connected and indexed';
        this.item.backgroundColor = undefined;
      }
    } catch {
      this.item.text = '$(warning) NestWeaver';
      this.item.tooltip = 'NestWeaver: not running. Start with `nestweaver ui`';
      this.item.backgroundColor = new vscode.ThemeColor('statusBarItem.warningBackground');
    }
  }

  dispose() {
    if (this.timer) { clearInterval(this.timer); }
    this.item.dispose();
  }
}
