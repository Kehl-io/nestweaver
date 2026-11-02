export interface SearchAvailability {
  sources: string[];
  upstreams: { source: string; status: string }[];
  partial: boolean;
  freshness: string;
  read_only: boolean;
  truncated?: boolean;
}
export interface SearchEnvelope<T> { results: T[]; _federation?: SearchAvailability; }
export function searchEnvelope<T>(value: T[] | SearchEnvelope<T>): SearchEnvelope<T> {
  return Array.isArray(value) ? { results: value } : value;
}
