import {
  createContext,
  useContext,
  useEffect,
  useId,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
  type Dispatch,
  type SetStateAction,
} from "react";
import {
  IssueOpenedIcon,
  CircleIcon,
  GitPullRequestIcon,
  TagIcon,
  MilestoneIcon,
} from "@primer/octicons-react";
import { PanelLeftCloseIcon, PanelLeftOpenIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Link } from "@/components/navigation/link";
import { type Repository } from "@/lib/api";
import { useBrowserPreference } from "@/lib/browser-preference";
import { repoHref } from "@/lib/repository-route";
import { discussionQuery } from "./list";

type Selection = { view: "issues" | "pulls"; query: string; mine: boolean };
const Navigation = createContext<Dispatch<
  SetStateAction<Selection | undefined>
> | null>(null);

export function useDiscussionNavigation(
  view: "issues" | "pulls",
  query: string,
  mine: boolean | undefined,
) {
  const report = useContext(Navigation);
  useEffect(() => {
    if (mine !== undefined) report?.({ view, query, mine });
  }, [view, query, mine, report]);
}

export function DiscussionWorkspace({
  repo,
  url,
  view,
  showNavigation,
  children,
}: {
  repo: Repository;
  url: URL;
  view: "issues" | "pulls" | "labels" | "milestones";
  showNavigation: boolean;
  children: ReactNode;
}) {
  const id = useId();
  const [selection, report] = useState<Selection>();
  const sidebarElement = useRef<HTMLElement>(null);
  const [sidebar, setSidebar] = useBrowserPreference(
    "crab-discussion-sidebar",
    ["expanded", "collapsed"] as const,
  );
  useLayoutEffect(() => {
    const element = sidebarElement.current;
    if (!element) return;
    // Diagnostic headers and narrow header wrapping change the available height.
    // Measure the sticky position so the collapse action stays in the viewport.
    const update = () => {
      const top = Math.max(0, element.getBoundingClientRect().top);
      element.style.setProperty(
        "--discussion-sidebar-height",
        `${Math.max(200, innerHeight - top)}px`,
      );
    };
    update();
    const observer = new ResizeObserver(update);
    observer.observe(document.body);
    window.addEventListener("resize", update);
    window.addEventListener("scroll", update, { passive: true });
    return () => {
      observer.disconnect();
      window.removeEventListener("resize", update);
      window.removeEventListener("scroll", update);
    };
  }, [showNavigation]);
  if (!showNavigation) return children;
  const collapsed = sidebar === "collapsed";
  const pulls = view === "pulls";
  const label = pulls ? "Pull request navigation" : "Issue navigation";
  const toggle = collapsed ? "Expand sidebar" : "Collapse sidebar";
  const ToggleIcon = collapsed ? PanelLeftOpenIcon : PanelLeftCloseIcon;
  const query = discussionQuery(url, pulls ? "pulls" : "issues");
  const mine =
    selection?.view === view && selection.query === query
      ? selection.mine
      : undefined;
  const root = pulls ? "pulls" : "issues";
  const links = [
    {
      view: pulls ? "pulls" : "issues",
      label: pulls ? "Pull requests" : "Issues",
      Icon: pulls ? GitPullRequestIcon : IssueOpenedIcon,
      active:
        view === root &&
        (mine === false || (!url.searchParams.get("q") && mine !== true)),
      query: undefined,
    },
    {
      view: root,
      label: pulls ? "Authored by me" : "Created by me",
      Icon: CircleIcon,
      active: view === root && mine === true,
      query: pulls ? "author:@me is:open" : "is:issue author:@me is:open",
    },
    {
      view: "milestones",
      label: "Milestones",
      Icon: MilestoneIcon,
      active: view === "milestones",
      query: undefined,
    },
    {
      view: "labels",
      label: "Labels",
      Icon: TagIcon,
      active: view === "labels",
      query: undefined,
    },
  ];
  return (
    <Navigation.Provider value={report}>
      <div className="discussion-workspace" data-sidebar={sidebar}>
        <aside
          ref={sidebarElement}
          className="discussion-sidebar"
          aria-label={label}
        >
          <nav id={id} aria-label={label}>
            {links.map(({ view: target, label, Icon, active, query }) => (
              <Tooltip key={label}>
                <TooltipTrigger asChild>
                  <Link
                    className={active ? "active" : ""}
                    aria-label={label}
                    aria-current={active ? "page" : undefined}
                    href={repoHref(repo, { view: target, q: query })}
                  >
                    <Icon aria-hidden="true" />
                    <span className="discussion-sidebar-label">{label}</span>
                  </Link>
                </TooltipTrigger>
                {collapsed && (
                  <TooltipContent side="right">{label}</TooltipContent>
                )}
              </Tooltip>
            ))}
          </nav>
          <Tooltip>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                className="discussion-sidebar-toggle"
                aria-label={toggle}
                aria-controls={id}
                aria-expanded={!collapsed}
                onClick={() => setSidebar(collapsed ? "expanded" : "collapsed")}
              >
                <ToggleIcon data-icon="inline-start" />
                <span className="discussion-sidebar-label">{toggle}</span>
              </Button>
            </TooltipTrigger>
            {collapsed && (
              <TooltipContent side="right">{toggle}</TooltipContent>
            )}
          </Tooltip>
        </aside>
        <div className="discussion-workspace-main">{children}</div>
      </div>
    </Navigation.Provider>
  );
}
