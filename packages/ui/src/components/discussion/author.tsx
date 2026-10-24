import { Link } from "@/components/navigation/link";
import type { ActorProfile } from "@/features/profile/model";

export interface DiscussionAuthor {
  name: string;
  profile: Pick<ActorProfile, "id" | "handle" | "name"> | null;
}

export function Author({ author }: { author: DiscussionAuthor }) {
  if (!author.profile) return <span>{author.name}</span>;
  return (
    <Link
      className="discussion-author"
      href={`/-/users/${encodeURIComponent(author.profile.id)}`}
      title={author.profile.name}
    >
      {author.profile.handle}
    </Link>
  );
}
