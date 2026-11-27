/**
 * Root & Feature React Error Boundaries (`P1-T31` / `Vortex-Architecture-Design.md` §23.5).
 *
 * Isolates unexpected UI render faults so sibling panels and the background Rust download engine
 * continue running uninterrupted, while logging sanitized details + `trace_id` to `core.jsonl`.
 */

import { Component, type ErrorInfo, type ReactNode } from "react";

interface BoundaryProps {
  readonly featureName: string;
  readonly isRoot?: boolean;
  readonly onReportError?: (component: string, message: string) => Promise<string>;
  readonly children: ReactNode;
}

interface BoundaryState {
  readonly hasError: boolean;
  readonly errorMessage: string;
  readonly traceId: string;
}

export class VortexErrorBoundary extends Component<BoundaryProps, BoundaryState> {
  public override state: BoundaryState = {
    hasError: false,
    errorMessage: "",
    traceId: "",
  };

  public static getDerivedStateFromError(error: unknown): Partial<BoundaryState> {
    return {
      hasError: true,
      errorMessage: error instanceof Error ? error.message : String(error),
    };
  }

  public override componentDidCatch(error: unknown, info: ErrorInfo): void {
    const detail = `${error instanceof Error ? error.message : String(error)} | stack=${info.componentStack ?? ""}`;
    if (this.props.onReportError !== undefined) {
      void this.props.onReportError(this.props.featureName, detail).then((tid) => {
        this.setState({ traceId: tid });
      });
    }
  }

  private readonly handleReset = (): void => {
    this.setState({ hasError: false, errorMessage: "", traceId: "" });
  };

  public override render(): ReactNode {
    if (!this.state.hasError) {
      return this.props.children;
    }

    return (
      <section
        className="vtx-card"
        role="alert"
        style={{
          padding: "20px",
          borderColor: "var(--vtx-status-danger)",
          display: "flex",
          flexDirection: "column",
          gap: "12px",
        }}
      >
        <strong style={{ color: "var(--vtx-status-danger)" }}>
          {this.props.isRoot === true
            ? "حدث خطأ غير متوقع في واجهة العرض — محرك التحميل في الخلفية مستمر بأمان"
            : `تعذر عرض لوحة (${this.props.featureName}) مؤقتاً`}
        </strong>
        <div style={{ color: "var(--vtx-text-secondary)", fontSize: "12px" }}>
          <span className="vtx-ltr-isolate">{this.state.errorMessage}</span>
          {this.state.traceId.length > 0 && (
            <div>
              Trace ID: <code className="vtx-ltr-isolate">{this.state.traceId}</code>
            </div>
          )}
        </div>
        <div>
          <button type="button" className="vtx-btn vtx-btn-primary" onClick={this.handleReset}>
            إعادة تحميل الواجهة
          </button>
        </div>
      </section>
    );
  }
}
