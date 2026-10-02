// Everything a page needs, from one import:
//
//   import { html, useState } from '../../vendor/preact-htm.js';
//   import { Page, Panel, Table, Button, toast } from '../components/index.js';
//
// The per-file modules stay importable on their own; this barrel only saves
// typing. See ui/UI_GUIDE.md for props and examples.

export { Icon, LogoMark, ICON_NAMES } from './icons.js';
export { Button, IconButton, CopyButton, Spinner } from './button.js';
export { StatusLamp, Badge, Kbd, toneForStatus, TONES } from './status.js';
export {
  Page,
  Panel,
  Card,
  Notice,
  Stat,
  StatGroup,
  KeyValue,
  Skeleton,
  EmptyState,
  ErrorState,
  Pagination,
  LoadMore,
  Timeline,
} from './surface.js';
export { Table, sortRows } from './table.js';
export { Tabs, Segmented } from './nav.js';
export {
  Field,
  Input,
  Textarea,
  Select,
  Switch,
  Checkbox,
  NumberInput,
  TagInput,
  SecretInput,
  Form,
  FormRow,
  FormActions,
  FormError,
  useIssues,
} from './form.js';
export { Portal } from './portal.js';
export { Tooltip } from './tooltip.js';
export { Menu } from './menu.js';
export { Modal, Drawer, ConfirmDialog, ConfirmHost, confirm } from './overlay.js';
export { toast, Toaster } from './toast.js';
export { CodeBlock, formatJson, highlightJson } from './code.js';
export {
  Sparkline,
  LineChart,
  AreaChart,
  BarChart,
  BarList,
  LatencyBars,
  Meter,
  HealthStrip,
  seriesColor,
  foldSeries,
  niceScale,
} from './charts.js';
export { TrackDiagram } from './track.js';
