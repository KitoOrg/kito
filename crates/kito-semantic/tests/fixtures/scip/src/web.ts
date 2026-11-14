import { getUser } from "./auth";

export function profile(id: string): string {
  return getUser(id);
}
