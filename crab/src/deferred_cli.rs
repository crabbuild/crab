use clap::builder::Resettable;
use clap::{ArgMatches, Args, Command, Error, FromArgMatches, Id, Subcommand};

// Completion generation inspects the complete tree before Clap builds deferred
// descendants. Both modes use the same argument definitions and match conversion.
pub(crate) struct Deferred<T, const DEFER: bool = true>(pub(crate) T);

fn augment(command: Command, build: fn(Command) -> Command) -> Command {
    // Variant docs follow eager augmentation. Deferral runs later, so retain
    // their precedence rather than exposing the argument type's internal docs.
    let about = Resettable::from(command.get_about().cloned());
    let long_about = Resettable::from(command.get_long_about().cloned());
    build(command).about(about).long_about(long_about)
}

impl<T: FromArgMatches, const DEFER: bool> FromArgMatches for Deferred<T, DEFER> {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, Error> {
        T::from_arg_matches(matches).map(Self)
    }

    fn from_arg_matches_mut(matches: &mut ArgMatches) -> Result<Self, Error> {
        T::from_arg_matches_mut(matches).map(Self)
    }

    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), Error> {
        self.0.update_from_arg_matches(matches)
    }

    fn update_from_arg_matches_mut(&mut self, matches: &mut ArgMatches) -> Result<(), Error> {
        self.0.update_from_arg_matches_mut(matches)
    }
}

impl<T: Args, const DEFER: bool> Args for Deferred<T, DEFER> {
    fn group_id() -> Option<Id> {
        T::group_id()
    }

    fn augment_args(command: Command) -> Command {
        // Keep the original parser and group identity; unused commands need
        // no argument tree, while selected commands retain Clap's normal build.
        if DEFER {
            command.defer(|command| augment(command, T::augment_args))
        } else {
            T::augment_args(command)
        }
    }

    fn augment_args_for_update(command: Command) -> Command {
        if DEFER {
            command.defer(|command| augment(command, T::augment_args_for_update))
        } else {
            T::augment_args_for_update(command)
        }
    }
}

impl<T: Subcommand, const DEFER: bool> Subcommand for Deferred<T, DEFER> {
    fn augment_subcommands(command: Command) -> Command {
        if DEFER {
            command.defer(|command| augment(command, T::augment_subcommands))
        } else {
            T::augment_subcommands(command)
        }
    }

    fn augment_subcommands_for_update(command: Command) -> Command {
        if DEFER {
            command.defer(|command| augment(command, T::augment_subcommands_for_update))
        } else {
            T::augment_subcommands_for_update(command)
        }
    }

    fn has_subcommand(name: &str) -> bool {
        T::has_subcommand(name)
    }
}
