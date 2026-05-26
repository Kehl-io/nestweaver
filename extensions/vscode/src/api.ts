import * as http from "http";

export class NestWeaverApi {
  constructor(private baseUrl: string) {}

  get<T>(path: string): Promise<T> {
    return new Promise((resolve, reject) => {
      http.get(`${this.baseUrl}${path}`, (res) => {
        let data = "";
        res.on("data", (chunk) => (data += chunk));
        res.on("end", () => {
          try { resolve(JSON.parse(data)); }
          catch { reject(new Error(`Parse failed: ${path}`)); }
        });
        res.on("error", reject);
      }).on("error", reject);
    });
  }

  health() { return this.get<{ status: string }>("/api/v1/health"); }
  search(q: string, limit = 10) { return this.get<any[]>(`/api/v1/search?q=${encodeURIComponent(q)}&limit=${limit}`); }
  symbol(uid: string) { return this.get<any>(`/api/v1/symbol/${encodeURIComponent(uid)}`); }
}
