export interface ActorProfile {
  id: string;
  handle: string;
  name: string;
  version: number;
  created_at_ms: number;
  updated_at_ms: number;
}
export interface OwnProfile {
  profile: ActorProfile | null;
}
