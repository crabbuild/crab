import { Link } from "@/components/navigation/link";
import { UserAvatar } from "@/components/ui/user-avatar";
import type { ActorProfile } from "@/lib/contracts";

export interface DiscussionAuthor {
  name: string;
  profile: Pick<ActorProfile, "id" | "handle" | "name" | "avatar"> | null;
}

export function Author({
  author,
  showAvatar = false,
}: {
  author: DiscussionAuthor;
  showAvatar?: boolean;
}) {
  const avatarNode = showAvatar ? (
    <UserAvatar
      name={author.profile?.handle || author.profile?.name || author.name}
      avatar={author.profile?.avatar}
      size={20}
    />
  ) : null;
  if (!author.profile) {
    return showAvatar ? (
      <span className="inline-flex items-center gap-1.5">
        {avatarNode}
        <span>{author.name}</span>
      </span>
    ) : (
      <span>{author.name}</span>
    );
  }
  return (
    <Link
      className={
        showAvatar
          ? "discussion-author inline-flex items-center gap-1.5"
          : "discussion-author"
      }
      href={`/-/users/${encodeURIComponent(author.profile.id)}`}
      title={author.profile.name}
    >
      {avatarNode}
      <span>{author.profile.handle}</span>
    </Link>
  );
}
