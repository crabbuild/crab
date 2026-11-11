import { Component, createRef, type ReactNode } from "react";

interface Props {
  storageKey: string;
  filtered: boolean;
  children: ReactNode;
}

export class RepositorySidebarScroll extends Component<Props> {
  private list = createRef<HTMLDivElement>();
  private position = 0;
  private observedTop = 0;

  componentDidMount() {
    this.position = Number(localStorage.getItem(this.props.storageKey)) || 0;
    this.restore();
    window.addEventListener("beforeunload", this.save);
  }

  getSnapshotBeforeUpdate(previous: Props) {
    // Filtering can clamp the DOM offset before layout effects run. Snapshot
    // the outgoing list before mutation, even when its scroll event is queued.
    this.capture(previous);
    if (previous.storageKey !== this.props.storageKey || previous.filtered !== this.props.filtered) {
      localStorage.setItem(previous.storageKey, String(this.position));
    }
    return null;
  }

  componentDidUpdate(previous: Props) {
    if (previous.storageKey !== this.props.storageKey) {
      this.position = Number(localStorage.getItem(this.props.storageKey)) || 0;
    }
    this.restore();
  }

  componentWillUnmount() {
    this.save();
    window.removeEventListener("beforeunload", this.save);
  }

  private capture = (props: Props = this.props) => {
    const top = this.list.current?.scrollTop;
    if (props.filtered || top === undefined) return;
    // An initially short list may clamp a saved offset while discovery loads.
    // Only a changed visible offset replaces that pending restoration.
    if (top !== this.observedTop) this.position = top;
    this.observedTop = top;
  };

  private restore() {
    const list = this.list.current;
    if (!list) return;
    list.scrollTop = this.props.filtered ? 0 : this.position;
    this.observedTop = list.scrollTop;
  }

  private save = () => {
    this.capture();
    localStorage.setItem(this.props.storageKey, String(this.position));
  };

  render() {
    return (
      <div ref={this.list} aria-label="Repository list" className="min-h-0 flex-1 overflow-auto" onScroll={() => this.capture()}>
        {this.props.children}
      </div>
    );
  }
}
