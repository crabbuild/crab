export interface ChartView {
  finalize(): void;
  addSignalListener(
    name: string,
    listener: (name: string, value: unknown) => void,
  ): ChartView;
  signal(name: string, value: unknown): ChartView;
  runAsync(): Promise<unknown>;
}
export function render(
  target: Element | string,
  spec: unknown,
  options: {
    actions?: boolean;
    interaction?: ChartInteraction | null;
    onError: (error: Error) => void;
  },
): Promise<{
  view: ChartView;
  spec: unknown;
  actions?: HTMLElement;
  finalize(): void;
}>;
import type {
  AxisRange,
  ChartInteraction,
} from "../../../packages/workflow-results/contracts";
export function validRange(domain: AxisRange, range: AxisRange): boolean;
export function zoomRange(
  domain: AxisRange,
  range: AxisRange | null,
  factor: number,
): AxisRange | null;
