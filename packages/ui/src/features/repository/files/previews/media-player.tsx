import { useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import { FieldSelect } from "@/components/ui/field-select";
import { Slider } from "@/components/ui/slider";
import {
  MaximizeIcon,
  PauseIcon,
  PlayIcon,
  Volume2Icon,
  VolumeXIcon,
} from "lucide-react";

const speeds = ["0.5", "1", "1.25", "1.5", "2"].map((value) => ({
  value,
  label: `${value}×`,
}));

function timestamp(seconds: number) {
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${String(Math.floor(seconds) % 60).padStart(2, "0")}`;
}

export function MediaPlayer({
  src,
  name,
  video,
}: {
  src: string;
  name: string;
  video: boolean;
}) {
  const media = useRef<HTMLMediaElement | null>(null);
  const container = useRef<HTMLDivElement>(null);
  const [playing, setPlaying] = useState(false);
  const [duration, setDuration] = useState(0);
  const [position, setPosition] = useState(0);
  const [volume, setVolume] = useState(100);
  const [muted, setMuted] = useState(false);
  const [feedback, setFeedback] = useState("");
  const Media = video ? "video" : "audio";
  const label = `${video ? "Video" : "Audio"} preview of ${name}`;

  return (
    <div
      ref={container}
      className="media-player"
      role="region"
      aria-label={label}
    >
      <Media
        ref={(element: HTMLMediaElement | null) => {
          media.current = element;
        }}
        src={src}
        preload="metadata"
        playsInline={video}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onDurationChange={({ currentTarget }) =>
          setDuration(
            Number.isFinite(currentTarget.duration)
              ? currentTarget.duration
              : 0,
          )
        }
        onTimeUpdate={({ currentTarget }) =>
          setPosition(currentTarget.currentTime)
        }
        onVolumeChange={({ currentTarget }) => {
          setMuted(currentTarget.muted);
          setVolume(Math.round(currentTarget.volume * 100));
        }}
        onError={() => setFeedback("This file could not be played.")}
      />
      <div className="flex w-full max-w-2xl flex-wrap items-center gap-3">
        <Button
          variant="outline"
          size="icon"
          aria-label={playing ? "Pause" : "Play"}
          onClick={() => {
            const player = media.current;
            if (!player) return;
            setFeedback("");
            if (player.paused)
              void player
                .play()
                .catch(() => setFeedback("This file could not be played."));
            else player.pause();
          }}
        >
          {playing ? <PauseIcon /> : <PlayIcon />}
        </Button>
        <Slider
          className="min-w-24 flex-1"
          label="Playback position"
          min={0}
          max={duration || 1}
          step={0.1}
          disabled={!duration}
          value={Math.min(position, duration)}
          onValueChange={(value) => {
            if (media.current) media.current.currentTime = value;
          }}
        />
        <span className="font-mono text-xs tabular-nums">
          {timestamp(position)} / {timestamp(duration)}
        </span>
        <Button
          variant="outline"
          size="icon"
          aria-label={muted ? "Unmute" : "Mute"}
          onClick={() => {
            if (media.current) media.current.muted = !muted;
          }}
        >
          {muted ? <VolumeXIcon /> : <Volume2Icon />}
        </Button>
        <Slider
          className="w-16"
          label="Volume"
          min={0}
          max={100}
          value={muted ? 0 : volume}
          onValueChange={(value) => {
            if (!media.current) return;
            media.current.volume = value / 100;
            media.current.muted = false;
          }}
        />
        <FieldSelect
          label="Playback speed"
          defaultValue="1"
          options={speeds}
          onValueChange={(value) => {
            if (media.current) media.current.playbackRate = Number(value);
          }}
        />
        {video && document.fullscreenEnabled && (
          <Button
            variant="outline"
            size="icon"
            aria-label="Toggle fullscreen"
            onClick={() => {
              const change = document.fullscreenElement
                ? document.exitFullscreen()
                : container.current?.requestFullscreen();
              void change?.catch(() =>
                setFeedback("Fullscreen is unavailable."),
              );
            }}
          >
            <MaximizeIcon />
          </Button>
        )}
      </div>
      {feedback && (
        <p role="status" className="text-sm text-destructive">
          {feedback}
        </p>
      )}
    </div>
  );
}
