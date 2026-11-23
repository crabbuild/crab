import {
  ArrowRightIcon,
  CircleAlertIcon,
  LogInIcon,
  UserRoundPlusIcon,
} from "lucide-react";
import { MarkGithubIcon } from "@primer/octicons-react";
import { Button } from "@/components/ui/button";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
} from "@/components/ui/card";
import type { Session } from "@/lib/contracts";
import artwork from "./sign-in-art.svg";
import "./sign-in.css";

export function SignIn({
  mode,
  canSignUp,
  url,
}: {
  mode: Session["mode"];
  canSignUp: boolean;
  url: URL;
}) {
  const github = mode === "github";
  const query = new URLSearchParams(url.search);
  const failed = query.has("auth_error");
  query.delete("auth_error");
  const search = query.toString();
  const returnTo = search ? `${url.pathname}?${search}` : url.pathname;
  const login = `/auth/login?${new URLSearchParams({ return_to: returnTo })}`;
  const signUp = `${login}&sign_up=true`;

  return (
    <section className="sign-in" aria-labelledby="sign-in-title">
      <Card className="sign-in-content">
        <CardHeader className="sign-in-header">
          <h1 id="sign-in-title">Sign in to Crab</h1>
          <CardDescription>
            {github
              ? "Use your GitHub account to access Crab."
              : "Access your repositories, issues, and pull requests."}
          </CardDescription>
        </CardHeader>
        <CardContent>
          {failed && (
            <p className="sign-in-error" role="alert">
              <CircleAlertIcon aria-hidden="true" />
              <span>
                We couldn’t sign you in. Try again or contact your
                administrator.
              </span>
            </p>
          )}
          <div className="sign-in-actions">
            <Button asChild className="sign-in-action">
              <a href={login}>
                {github ? (
                  <MarkGithubIcon aria-hidden="true" />
                ) : (
                  <LogInIcon aria-hidden="true" />
                )}
                {github ? "Continue with GitHub" : "Continue to sign in"}
                <ArrowRightIcon className="ml-auto" aria-hidden="true" />
              </a>
            </Button>
            {github || canSignUp ? (
              <Button asChild variant="outline" className="sign-in-action">
                <a href={github ? "https://github.com/signup" : signUp}>
                  <UserRoundPlusIcon aria-hidden="true" />
                  {github ? "Sign up with GitHub" : "Sign up"}
                </a>
              </Button>
            ) : null}
          </div>
          <p className="sign-in-help">
            {github || canSignUp
              ? "For access to a team repository, ask its administrator."
              : "If your provider offers registration, you can sign up on its sign-in page."}
          </p>
        </CardContent>
      </Card>
      <div className="sign-in-visual" aria-hidden="true">
        <img src={artwork} width={720} height={600} alt="" />
      </div>
    </section>
  );
}
