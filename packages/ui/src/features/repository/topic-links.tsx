import { Link } from "@/components/navigation/link";
import { Badge } from "@/components/ui/badge";

const browseTopic = (topic: string) =>
  `/?${new URLSearchParams({ q: `topic:${topic}` })}`;

export function TopicLinks({
  names,
  href = browseTopic,
}: {
  names?: string[];
  href?: (topic: string) => string;
}) {
  if (!names?.length) return null;

  return (
    <ul aria-label="Repository topics" className="my-2 flex flex-wrap gap-2">
      {names.map((topic) => (
        <li key={topic} className="min-w-0">
          <Badge
            asChild
            variant="secondary"
            className="max-w-full whitespace-normal break-all"
          >
            <Link href={href(topic)}>{topic}</Link>
          </Badge>
        </li>
      ))}
    </ul>
  );
}
