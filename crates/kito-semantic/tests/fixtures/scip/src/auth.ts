export function getUser(id: string): string {
  return id;
}

export function current(): string {
  return getUser("me");
}
