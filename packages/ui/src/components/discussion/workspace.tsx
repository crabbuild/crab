import { useId, useLayoutEffect, useRef, type ReactNode } from "react";
import {
  IssueOpenedIcon,
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

export function DiscussionWorkspace({
  repo,
  view,
  showNavigation,
  children,
}: {
  repo: Repository;
  view: "issues" | "pulls" | "labels" | "milestones";
  showNavigation: boolean;
  children: ReactNode;
}) {
  const id = useId();
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
  const links = [
    {
      view: pulls ? "pulls" : "issues",
      label: pulls ? "Pull requests" : "Issues",
      Icon: pulls ? GitPullRequestIcon : IssueOpenedIcon,
    },
    { view: "milestones", label: "Milestones", Icon: MilestoneIcon },
    { view: "labels", label: "Labels", Icon: TagIcon },
  ];
  return (
    <div className="discussion-workspace" data-sidebar={sidebar}>
      <aside
        ref={sidebarElement}
        className="discussion-sidebar"
        aria-label={label}
      >
        <nav id={id} aria-label={label}>
          {links.map(({ view: target, label, Icon }) => (
            <Tooltip key={target}>
              <TooltipTrigger asChild>
                <Link
                  className={view === target ? "active" : ""}
                  aria-label={label}
                  aria-current={view === target ? "page" : undefined}
                  href={repoHref(repo, { view: target })}
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
          {collapsed && <TooltipContent side="right">{toggle}</TooltipContent>}
        </Tooltip>
      </aside>
      <div className="discussion-workspace-main">{children}</div>
    </div>
  );
}
