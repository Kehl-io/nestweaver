import * as vscode from 'vscode';
import { NestWeaverApi } from './api';

export function registerChatParticipant(context: vscode.ExtensionContext, api: NestWeaverApi) {
  const participant = vscode.chat.createChatParticipant('nestweaver', async (request, _ctx, stream, _token) => {
    const query = request.prompt.toLowerCase();

    if (query.includes('architecture') || query.includes('overview') || query.includes('guide')) {
      stream.markdown('Fetching codebase architecture...\n\n');
      try {
        const guide = await api.guide();
        stream.markdown(guide);
      } catch (e: any) {
        stream.markdown(`Error: ${e.message}`);
      }
      return;
    }

    if (query.includes('impact') || query.includes('blast radius') || query.includes('what calls')) {
      const symbolMatch = query.match(/(?:impact|calls|callers of|depends on)\s+(\w+)/i);
      const symbol = symbolMatch?.[1] || request.prompt.split(' ').pop() || '';
      stream.markdown(`Analyzing impact of \`${symbol}\`...\n\n`);
      try {
        const result = await api.impact(symbol);
        stream.markdown(`**Impact of ${symbol}** (${result.nodes?.length ?? 0} affected symbols)\n\n`);
        for (const node of (result.nodes || []).slice(0, 10)) {
          stream.markdown(`- \`${node.name}\` (${node.file_path}:${node.start_line})\n`);
        }
      } catch (e: any) {
        stream.markdown(`Error: ${e.message}`);
      }
      return;
    }

    if (query.includes('search') || query.includes('find')) {
      const searchTerm = request.prompt.replace(/^(search|find)\s+/i, '').trim();
      stream.markdown(`Searching for \`${searchTerm}\`...\n\n`);
      try {
        const results = await api.search(searchTerm);
        for (const r of results.slice(0, 10)) {
          stream.markdown(`- **${r.name}** (${r.kind}) — ${r.file_path}:${r.start_line}\n`);
        }
      } catch (e: any) {
        stream.markdown(`Error: ${e.message}`);
      }
      return;
    }

    // Default: use brain_context
    const seeds = request.prompt.split(/\s+/).filter((w: string) => w.length > 2);
    stream.markdown(`Getting context for: ${seeds.join(', ')}...\n\n`);
    try {
      const result = await api.context(seeds);
      stream.markdown(`**Seeds resolved:** ${result.seeds?.length ?? 0}\n`);
      stream.markdown(`**Connected symbols:** ${result.connected?.length ?? 0}\n\n`);
      for (const c of (result.connected || []).slice(0, 15)) {
        stream.markdown(`- ${c.kind}: \`${c.name}\` (${c.location})\n`);
      }
    } catch (e: any) {
      stream.markdown(`Error: ${e.message}`);
    }
  });

  participant.iconPath = vscode.Uri.joinPath(context.extensionUri, 'icon.png');
  context.subscriptions.push(participant);
}
