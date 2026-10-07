import { expect, type Page } from "@playwright/test";

type Receipts = { replies: string[]; events: string[] };

// Observe the application's actual JSON decoder and native event dispatch,
// rather than treating route.fulfill() as proof the browser processed a reply.
export async function installDeliveryReceipts(page: Page) {
  await page.addInitScript(() => {
    const receipts = { replies: [] as string[], events: [] as string[] };
    Object.assign(window, { deliveryReceipts: receipts });
    const fetch = window.fetch.bind(window);
    window.fetch = async (...args) => {
      const response = await fetch(...args);
      const json = response.json.bind(response);
      response.json = async () => {
        const body: unknown = await json();
        receipts.replies.push(response.url);
        return body;
      };
      return response;
    };
    const NativeEventSource = window.EventSource;
    window.EventSource = class extends NativeEventSource {
      constructor(url: string | URL, options?: EventSourceInit) {
        super(url, options);
        for (const type of ["watcher:status", "graph:updated", "full_refresh", "graph:generation"]) {
          this.addEventListener(type, () => receipts.events.push(type));
        }
      }
    };
  });
}

export async function decodedReplyCount(page: Page, path: string, params: Record<string, string> = {}) {
  return page.evaluate(({ path, params }) => {
    const receipts = (window as unknown as { deliveryReceipts: Receipts }).deliveryReceipts;
    return receipts.replies.filter((reply) => {
      const url = new URL(reply);
      return url.pathname === path && Object.entries(params).every(([key, value]) => url.searchParams.get(key) === value);
    }).length;
  }, { path, params });
}

export async function renderedFrame(page: Page) {
  // The decoder's caller resumes in microtasks before the next painted frame.
  await page.evaluate(() => new Promise<void>((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve()))));
}

export async function waitForDecodedReply(page: Page, path: string, params: Record<string, string>, baseline: number) {
  await expect.poll(() => decodedReplyCount(page, path, params), { message: "browser decoded the released reply" }).toBeGreaterThan(baseline);
  await renderedFrame(page);
}

export async function eventCount(page: Page, type: string) {
  return page.evaluate((type) => (window as unknown as { deliveryReceipts: Receipts }).deliveryReceipts.events.filter((event) => event === type).length, type);
}
